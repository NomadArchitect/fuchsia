// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::client::config_management::Credential;
use crate::client::roaming::lib::*;
use crate::client::roaming::roam_monitor::RoamMonitorApi;
use crate::client::types;
use crate::config_management::SavedNetworksManagerApi;
use crate::telemetry::{TelemetryEvent, TelemetrySender};
use crate::util::pseudo_energy::EwmaSignalData;
use async_trait::async_trait;
use futures::lock::Mutex;
use log::{error, info};
use std::sync::Arc;
use {
    fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_internal as fidl_internal,
    fuchsia_async as fasync,
};

pub const MIN_BACKOFF_BETWEEN_ROAM_SCANS_ABSOLUTE: zx::MonotonicDuration =
    zx::MonotonicDuration::from_minutes(1);
pub const MIN_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE: zx::MonotonicDuration =
    zx::MonotonicDuration::from_minutes(2);
pub const MAX_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE: zx::MonotonicDuration =
    zx::MonotonicDuration::from_minutes(32);

const LOCAL_ROAM_THRESHOLD_RSSI_2G: f64 = -72.0;
const LOCAL_ROAM_THRESHOLD_RSSI_5G: f64 = -75.0;

const MIN_RSSI_IMPROVEMENT_TO_ROAM: f64 = 3.0;

/// Number of previous RSSI measurements to exponentially weigh into average.
/// TODO(https://fxbug.dev/42165706): Tune smoothing factor.
pub const STATIONARY_ROAMING_EWMA_SMOOTHING_FACTOR: usize = 10;

// Roams will not be considered if more than this many roams have been attempted in the last day.
pub const NUM_MAX_ROAMS_PER_DAY: usize = 5;

pub struct StationaryMonitor {
    pub connection_data: RoamingConnectionData,
    pub telemetry_sender: TelemetrySender,
    saved_networks: Arc<dyn SavedNetworksManagerApi>,
    scan_backoff_if_no_change: zx::MonotonicDuration,
    /// To be used to limit how often roams can happen to avoid thrashing between APs.
    past_roams: Arc<Mutex<PastRoamList>>,
}

impl StationaryMonitor {
    pub fn new(
        ap_state: types::ApState,
        network_identifier: types::NetworkIdentifier,
        credential: Credential,
        telemetry_sender: TelemetrySender,
        saved_networks: Arc<dyn SavedNetworksManagerApi>,
        past_roams: Arc<Mutex<PastRoamList>>,
    ) -> Self {
        let connection_data = RoamingConnectionData::new(
            ap_state.clone(),
            network_identifier,
            credential,
            EwmaSignalData::new(
                ap_state.tracked.signal.rssi_dbm,
                ap_state.tracked.signal.snr_db,
                STATIONARY_ROAMING_EWMA_SMOOTHING_FACTOR,
            ),
        );
        Self {
            connection_data,
            telemetry_sender,
            saved_networks,
            scan_backoff_if_no_change: MIN_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE,
            past_roams,
        }
    }

    // Handle signal report indiciations. Update internal connection data, if necessary. Returns
    // true if a roam search should be initiated.
    #[allow(clippy::needless_return, reason = "mass allow for https://fxbug.dev/381896734")]
    async fn handle_signal_report(
        &mut self,
        stats: fidl_internal::SignalReportIndication,
    ) -> Result<RoamTriggerDataOutcome, anyhow::Error> {
        self.connection_data.signal_data.update_with_new_measurement(stats.rssi_dbm, stats.snr_db);

        // Update velocity with EWMA signal, to smooth out noise.
        self.connection_data.rssi_velocity.update(self.connection_data.signal_data.ewma_rssi.get());

        self.telemetry_sender.send(TelemetryEvent::OnSignalVelocityUpdate {
            rssi_velocity: self.connection_data.rssi_velocity.get(),
        });

        // If the network likely has 1 BSS, don't scan for another BSS to roam to.
        match self
            .saved_networks
            .is_network_single_bss(
                &self.connection_data.network_identifier,
                &self.connection_data.credential,
            )
            .await
        {
            Ok(true) => return Ok(RoamTriggerDataOutcome::Noop),
            _ => {
                // There could be an error if the config is not found. If there was an error, treat
                // that as the network could be multi BSS and consider a roam scan.
                return Ok(self.should_roam_scan_after_signal_report());
            }
        }
    }

    fn should_roam_scan_after_signal_report(&mut self) -> RoamTriggerDataOutcome {
        // If there have been too many roam attempts in the past 24 hours, do not attempt roaming.
        if let Some(past_roams) = self.past_roams.try_lock() {
            if past_roams
                .get_recent(fasync::MonotonicInstant::now() - TIMESPAN_TO_LIMIT_SCANS)
                .len()
                >= NUM_MAX_ROAMS_PER_DAY
            {
                return RoamTriggerDataOutcome::Noop;
            }
        } else {
            error!("Unexpectedly failed to get lock on recent roam attempts data");
        }

        // Determine any roam reasons based on the signal thresholds.
        let mut roam_reasons: Vec<RoamReason> = vec![];

        // Check RSSI threshold
        let rssi_threshold = if self.connection_data.ap_state.tracked.channel.is_5ghz() {
            LOCAL_ROAM_THRESHOLD_RSSI_5G
        } else {
            LOCAL_ROAM_THRESHOLD_RSSI_2G
        };
        let rssi = self.connection_data.signal_data.ewma_rssi.get();
        if rssi <= rssi_threshold {
            roam_reasons.push(RoamReason::RssiBelowThreshold);
        }

        // Do not scan if there are no roam reasons, or if the scan backoff has not yet passed.
        let now = fasync::MonotonicInstant::now();
        if roam_reasons.is_empty()
            || now
                < self.connection_data.previous_roam_scan_data.time_prev_roam_scan
                    + MIN_BACKOFF_BETWEEN_ROAM_SCANS_ABSOLUTE
        {
            return RoamTriggerDataOutcome::Noop;
        }

        let is_scan_old = now
            > self.connection_data.previous_roam_scan_data.time_prev_roam_scan
                + self.scan_backoff_if_no_change;
        let has_new_reason = roam_reasons.iter().any(|r| {
            !self.connection_data.previous_roam_scan_data.prev_roam_scan_reasons.contains(r)
        });

        // Only initiate roam search if there are new roam reasons, or the roam scan backoff has
        // passed.
        if is_scan_old || has_new_reason {
            if has_new_reason {
                // Reset roam scan backoff if there are new roam reasons.
                self.scan_backoff_if_no_change = MIN_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE;
                // Only update the roam reasons if there is a new reason, in case of roam reasons
                // that come and go.
                self.connection_data.previous_roam_scan_data.prev_roam_scan_reasons =
                    roam_reasons.clone();
            } else {
                // Otherwise, exponentially extend the roam scan backoff.
                self.scan_backoff_if_no_change = std::cmp::min(
                    self.scan_backoff_if_no_change * 2_i64,
                    MAX_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE,
                );
            }

            // Updated fields for tracking roam scan decisions and initiated roam search.
            self.connection_data.previous_roam_scan_data.time_prev_roam_scan =
                fasync::MonotonicInstant::now();
            self.connection_data.previous_roam_scan_data.rssi_prev_roam_scan = rssi;
            info!("Initiating roam search for roam reasons: {:?}", &roam_reasons);
            // Stationary monitor uses active roam scans to prioritize shorter scan times over power
            // consumption.
            return RoamTriggerDataOutcome::RoamSearch {
                scan_type: fidl_common::ScanType::Active,
                network_identifier: self.connection_data.network_identifier.clone(),
                credential: self.connection_data.credential.clone(),
                reasons: roam_reasons,
            };
        }
        RoamTriggerDataOutcome::Noop
    }
}

#[async_trait(?Send)]
impl RoamMonitorApi for StationaryMonitor {
    async fn handle_roam_trigger_data(
        &mut self,
        data: RoamTriggerData,
    ) -> Result<RoamTriggerDataOutcome, anyhow::Error> {
        match data {
            RoamTriggerData::SignalReportInd(stats) => self.handle_signal_report(stats).await,
        }
    }

    fn should_send_roam_request(&self, request: PolicyRoamRequest) -> Result<bool, anyhow::Error> {
        if request.candidate.bss.bssid == self.connection_data.ap_state.original().bssid {
            info!("Selected roam candidate is the currently connected candidate, ignoring");
            return Ok(false);
        }
        // Only send roam scan if the selected candidate shows a significant signal improvement,
        // compared to the most up-to-date roaming connection data
        let latest_rssi = self.connection_data.signal_data.ewma_rssi.get();
        if (request.candidate.bss.signal.rssi_dbm as f64)
            < latest_rssi + MIN_RSSI_IMPROVEMENT_TO_ROAM
        {
            info!(
                "Selected roam candidate ({:?}) is not enough of an improvement. Ignoring.",
                request.candidate.to_string_without_pii()
            );
            return Ok(false);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::util::testing::{
        generate_random_bss, generate_random_password, generate_random_roaming_connection_data,
        generate_random_scanned_candidate, FakeSavedNetworksManager,
    };
    use fidl_fuchsia_wlan_internal as fidl_internal;
    use futures::channel::mpsc;
    use futures::task::Poll;
    use test_case::test_case;
    use wlan_common::assert_variant;

    struct TestValues {
        monitor: StationaryMonitor,
        telemetry_receiver: mpsc::Receiver<TelemetryEvent>,
        saved_networks: Arc<FakeSavedNetworksManager>,
        past_roams: Arc<Mutex<PastRoamList>>,
    }

    const TEST_OK_SNR: f64 = 40.0;

    fn setup_test() -> TestValues {
        let connection_data = generate_random_roaming_connection_data();
        let (telemetry_sender, telemetry_receiver) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telemetry_sender);
        let saved_networks = Arc::new(FakeSavedNetworksManager::new());
        let past_roams = Arc::new(Mutex::new(PastRoamList::new(NUM_MAX_ROAMS_PER_DAY)));
        // Set the fake saved networks manager to respond that the network is not single BSS by
        // default since most tests are for cases where roaming should be considered.
        saved_networks.set_is_single_bss_response(false);
        let monitor = StationaryMonitor {
            connection_data,
            telemetry_sender,
            saved_networks: saved_networks.clone(),
            scan_backoff_if_no_change: MIN_BACKOFF_BETWEEN_ROAM_SCANS_ABSOLUTE,
            past_roams: past_roams.clone(),
        };
        TestValues { monitor, telemetry_receiver, saved_networks, past_roams }
    }

    fn setup_test_with_data(connection_data: RoamingConnectionData) -> TestValues {
        let (telemetry_sender, telemetry_receiver) = mpsc::channel::<TelemetryEvent>(100);
        let telemetry_sender = TelemetrySender::new(telemetry_sender);
        let saved_networks = Arc::new(FakeSavedNetworksManager::new());
        let past_roams = Arc::new(Mutex::new(PastRoamList::new(NUM_MAX_ROAMS_PER_DAY)));
        let monitor = StationaryMonitor {
            connection_data,
            telemetry_sender,
            saved_networks: saved_networks.clone(),
            scan_backoff_if_no_change: MIN_BACKOFF_BETWEEN_ROAM_SCANS_ABSOLUTE,
            past_roams: past_roams.clone(),
        };
        TestValues { monitor, telemetry_receiver, saved_networks, past_roams }
    }

    /// This runs handle_roam_trigger_data with run_until_stalled and expects it to finish.
    /// run_single_threaded cannot be used with fake time.
    fn run_handle_roam_trigger_data(
        exec: &mut fasync::TestExecutor,
        monitor: &mut StationaryMonitor,
        trigger_data: RoamTriggerData,
    ) -> RoamTriggerDataOutcome {
        return assert_variant!(exec.run_until_stalled(&mut monitor.handle_roam_trigger_data(trigger_data)), Poll::Ready(Ok(should_roam)) => {should_roam});
    }

    #[test_case(-80, true; "bad rssi")]
    #[test_case(-40, false; "good rssi")]
    #[fuchsia::test(add_test_attr = false)]
    fn test_handle_signal_report_trigger_data(rssi: i8, should_roam_search: bool) {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::now());

        // Generate initial connection data based on test case.
        let connection_data = RoamingConnectionData {
            signal_data: EwmaSignalData::new(rssi, TEST_OK_SNR, 10),
            ..generate_random_roaming_connection_data()
        };
        let mut test_values = setup_test_with_data(connection_data);

        // Advance the time so that we allow roam scanning,
        exec.set_fake_time(fasync::MonotonicInstant::after(fasync::MonotonicDuration::from_hours(
            1,
        )));

        // Generate trigger data that won't change the above values, and send to handle_roam_trigger_data
        // method.
        let trigger_data =
            RoamTriggerData::SignalReportInd(fidl_internal::SignalReportIndication {
                rssi_dbm: rssi,
                snr_db: TEST_OK_SNR as i8,
            });
        let result =
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone());

        if should_roam_search {
            assert_variant!(result, RoamTriggerDataOutcome::RoamSearch { .. });
        } else {
            assert_variant!(result, RoamTriggerDataOutcome::Noop);
        }
    }

    #[fuchsia::test]
    fn test_stationary_monitor_uses_active_scans() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::now());

        // Setup monitor with connection data that would trigger a roam scan due to RSSI below
        // threshold. Set the EWMA weights to 1 so the values can be easily changed later in tests.
        let rssi = LOCAL_ROAM_THRESHOLD_RSSI_5G - 1.0;
        let connection_data = RoamingConnectionData {
            signal_data: EwmaSignalData::new(rssi, TEST_OK_SNR, 1),
            ..generate_random_roaming_connection_data()
        };
        let mut test_values = setup_test_with_data(connection_data);

        // Generate trigger data with same signal values as initial, which would trigger a roam
        // search due to the below threshold RSSI.
        let trigger_data =
            RoamTriggerData::SignalReportInd(fidl_internal::SignalReportIndication {
                rssi_dbm: rssi as i8,
                snr_db: TEST_OK_SNR as i8,
            });

        // Advance the time so that we allow roam scanning
        exec.set_fake_time(fasync::MonotonicInstant::after(fasync::MonotonicDuration::from_hours(
            1,
        )));

        // Send trigger data, and verify that the roam scan type is Active.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::RoamSearch { scan_type: fidl_common::ScanType::Active, .. }
        );
    }

    #[fuchsia::test]
    fn test_minimum_time_between_roam_scans() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::now());

        // Setup monitor with connection data that would trigger a roam scan due to RSSI below
        // threshold. Set the EWMA weights to 1 so the values can be easily changed later in tests.
        let rssi = LOCAL_ROAM_THRESHOLD_RSSI_5G - 1.0;
        let connection_data = RoamingConnectionData {
            signal_data: EwmaSignalData::new(rssi, TEST_OK_SNR, 1),
            ..generate_random_roaming_connection_data()
        };
        let mut test_values = setup_test_with_data(connection_data);
        let trigger_data =
            RoamTriggerData::SignalReportInd(fidl_internal::SignalReportIndication {
                rssi_dbm: rssi as i8,
                snr_db: TEST_OK_SNR as i8,
            });

        // Send trigger data, and verify that we are not told to roam search, as the minimum wait
        // time has not passed.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::Noop
        );

        // Advance the time just past the absolute minimum wait time.
        exec.set_fake_time(fasync::MonotonicInstant::after(
            MIN_BACKOFF_BETWEEN_ROAM_SCANS_ABSOLUTE + fasync::MonotonicDuration::from_seconds(1),
        ));

        // Send the same trigger data, and verify that we are told to roam search.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::RoamSearch { .. }
        );
    }

    #[fuchsia::test]
    fn test_minimum_time_if_no_changes() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::now());

        // Setup monitor with connection data that would trigger a roam scan due to RSSI
        // below thresholds.
        let rssi = LOCAL_ROAM_THRESHOLD_RSSI_5G - 1.0;
        let connection_data = RoamingConnectionData {
            signal_data: EwmaSignalData::new(rssi, TEST_OK_SNR, 10),
            ..generate_random_roaming_connection_data()
        };
        let mut test_values = setup_test_with_data(connection_data);

        // Generate trigger data with same signal values as initial, which would trigger a roam scan
        // due to low RSSI.
        let trigger_data =
            RoamTriggerData::SignalReportInd(fidl_internal::SignalReportIndication {
                rssi_dbm: rssi as i8,
                snr_db: TEST_OK_SNR as i8,
            });

        // Advance the time so that we allow roam scanning.
        let initial_time =
            fasync::MonotonicInstant::after(fasync::MonotonicDuration::from_hours(1));
        exec.set_fake_time(initial_time);

        // Send trigger data, and verify that we would be told to roam scan.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::RoamSearch { .. }
        );

        // Advance the time so its past the minimum between roam scans, but the minimum time if
        // there are no changes.
        exec.set_fake_time(
            initial_time
                + MIN_BACKOFF_BETWEEN_ROAM_SCANS_ABSOLUTE
                + fasync::MonotonicDuration::from_seconds(1),
        );

        // Send identical trigger data, and verify that we don't scan.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::Noop
        );
    }

    #[fuchsia::test]
    fn test_roam_scans_after_backoff_exceeded() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::now());

        // Setup monitor with connection data that would trigger a roam scan due to RSSI below threshold.
        let rssi = LOCAL_ROAM_THRESHOLD_RSSI_5G - 1.0;
        let connection_data = RoamingConnectionData {
            signal_data: EwmaSignalData::new(rssi, TEST_OK_SNR, 10),
            ..generate_random_roaming_connection_data()
        };
        let mut test_values = setup_test_with_data(connection_data);

        // Generate trigger data with same signal values as initial, which would trigger a roam scan
        // due to low RSSI.
        let trigger_data =
            RoamTriggerData::SignalReportInd(fidl_internal::SignalReportIndication {
                rssi_dbm: rssi as i8,
                snr_db: TEST_OK_SNR as i8,
            });

        // No time has passed so no roam scan should be considered.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::Noop
        );

        // Advanced time past the absolute minimum wait time between roam scans.
        exec.set_fake_time(fasync::MonotonicInstant::after(
            MIN_BACKOFF_BETWEEN_ROAM_SCANS_ABSOLUTE + fasync::MonotonicDuration::from_seconds(1),
        ));
        // Send trigger data, and verify that we would be told to roam scan.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::RoamSearch { .. }
        );

        // Advanced time past the absolute minimum wait time between roam scans, but not past the
        // minimum time if there is no change.
        exec.set_fake_time(fasync::MonotonicInstant::after(
            MIN_BACKOFF_BETWEEN_ROAM_SCANS_ABSOLUTE + fasync::MonotonicDuration::from_seconds(1),
        ));

        // No roam scan should occur, since last scan too recent given there were no changes.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::Noop
        );

        // Advance the time so its past the time between roam scans, even if there are no changes.
        exec.set_fake_time(fasync::MonotonicInstant::after(
            MIN_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE
                + fasync::MonotonicDuration::from_seconds(1),
        ));

        // Send trigger data, and verify that we will now scan, despite no RSSI change, because
        // the last scan now is considered old.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::RoamSearch { .. }
        );

        // Advance the time so its past the time between roam scans, even if there are no changes.
        exec.set_fake_time(fasync::MonotonicInstant::after(
            MIN_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE
                + fasync::MonotonicDuration::from_seconds(1),
        ));

        // No roam scan should occur, because now there is an _additional_ backoff, due to repeated
        // actionless roam scans with no change.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::Noop
        );

        // Advanced again, past the additional backoff (2x the minimum roam scan backoff)
        exec.set_fake_time(fasync::MonotonicInstant::after(
            MIN_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE
                + fasync::MonotonicDuration::from_seconds(1),
        ));

        // Send trigger data, and verify that we will now scan, despite no RSSI change, because
        // the last scan now is considered old.
        assert_variant!(
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone()),
            RoamTriggerDataOutcome::RoamSearch { .. }
        );
    }

    #[fuchsia::test]
    fn test_should_send_roam_request() {
        let _exec = fasync::TestExecutor::new();
        let test_values = setup_test();

        // Get the randomized RSSI value.
        let current_rssi = test_values.monitor.connection_data.signal_data.ewma_rssi.get();

        // Verify that roam recommendations are blocked if RSSI is an insufficient
        // improvement.
        let candidate = types::ScannedCandidate {
            bss: types::Bss {
                signal: types::Signal {
                    rssi_dbm: (current_rssi + MIN_RSSI_IMPROVEMENT_TO_ROAM - 1.0) as i8,
                    snr_db: TEST_OK_SNR as i8,
                },
                ..generate_random_bss()
            },
            ..generate_random_scanned_candidate()
        };
        assert!(!test_values
            .monitor
            .should_send_roam_request(PolicyRoamRequest { candidate, reasons: vec![] })
            .expect("failed to check roam request"));

        // Verify that a roam recommendation is made if RSSI improvement exceeds threshold.
        let candidate = types::ScannedCandidate {
            bss: types::Bss {
                signal: types::Signal {
                    rssi_dbm: (current_rssi + MIN_RSSI_IMPROVEMENT_TO_ROAM) as i8,
                    snr_db: TEST_OK_SNR as i8,
                },
                ..generate_random_bss()
            },
            ..generate_random_scanned_candidate()
        };
        assert!(test_values
            .monitor
            .should_send_roam_request(PolicyRoamRequest { candidate, reasons: vec![] })
            .expect("failed to check roam request"));

        // Verify that roam recommendations are blocked if the selected candidate is the currently
        // connected BSS. Set signal values high enough to isolate the dedupe function.
        let candidate = types::ScannedCandidate {
            bss: types::Bss {
                signal: types::Signal {
                    rssi_dbm: (current_rssi + MIN_RSSI_IMPROVEMENT_TO_ROAM + 1.0) as i8,
                    snr_db: TEST_OK_SNR as i8,
                },
                bssid: test_values.monitor.connection_data.ap_state.original().bssid,
                ..generate_random_bss()
            },
            credential: generate_random_password(),
            ..generate_random_scanned_candidate()
        };
        assert!(!test_values
            .monitor
            .should_send_roam_request(PolicyRoamRequest { candidate, reasons: vec![] })
            .expect("failed to check roam reqeust"));
    }

    #[fuchsia::test]
    fn test_send_signal_velocity_metric_event() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::now());

        let connection_data = RoamingConnectionData {
            signal_data: EwmaSignalData::new(-40, 50, 1),
            ..generate_random_roaming_connection_data()
        };
        let mut test_values = setup_test_with_data(connection_data);
        test_values.saved_networks.set_is_single_bss_response(true);

        let trigger_data =
            RoamTriggerData::SignalReportInd(fidl_internal::SignalReportIndication {
                rssi_dbm: -80,
                snr_db: TEST_OK_SNR as i8,
            });
        let _ =
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone());

        assert_variant!(
            test_values.telemetry_receiver.try_next(),
            Ok(Some(TelemetryEvent::OnSignalVelocityUpdate { .. }))
        );
    }

    #[fuchsia::test]
    fn test_should_not_roam_scan_single_bss() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::now());

        let rssi = -80;
        let connection_data = RoamingConnectionData {
            signal_data: EwmaSignalData::new(rssi, TEST_OK_SNR, 10),
            ..generate_random_roaming_connection_data()
        };
        let mut test_values = setup_test_with_data(connection_data);

        // Set the FakeSavedNetworks manager to report the network as single BSS
        test_values.saved_networks.set_is_single_bss_response(true);

        // Advance the time so that we allow roam scanning,
        exec.set_fake_time(fasync::MonotonicInstant::after(fasync::MonotonicDuration::from_hours(
            1,
        )));

        let trigger_data =
            RoamTriggerData::SignalReportInd(fidl_internal::SignalReportIndication {
                rssi_dbm: rssi,
                snr_db: TEST_OK_SNR as i8,
            });
        let trigger_result =
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone());

        assert_eq!(trigger_result, RoamTriggerDataOutcome::Noop);
    }

    #[fuchsia::test]
    fn test_roam_not_considered_if_attempted_too_many_times_today() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        let first_roam_time = fasync::MonotonicInstant::now();
        exec.set_fake_time(first_roam_time);

        // Send a signal report that would trigger a roam scan if the limit were not hit.
        let rssi = LOCAL_ROAM_THRESHOLD_RSSI_5G - 5.0;
        let connection_data = RoamingConnectionData {
            signal_data: EwmaSignalData::new(rssi, TEST_OK_SNR, 10),
            ..generate_random_roaming_connection_data()
        };
        let mut test_values = setup_test_with_data(connection_data);

        // Record enough roam attempts to prevent roaming for a while.
        for _ in 0..NUM_MAX_ROAMS_PER_DAY {
            test_values.past_roams.try_lock().unwrap().add(RoamEvent::new_roam_now());
            exec.set_fake_time(fasync::MonotonicInstant::after(
                MAX_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE,
            ));
        }

        let trigger_data =
            RoamTriggerData::SignalReportInd(fidl_internal::SignalReportIndication {
                rssi_dbm: rssi as i8,
                snr_db: TEST_OK_SNR as i8,
            });

        exec.set_fake_time(fasync::MonotonicInstant::after(
            MAX_BACKOFF_BETWEEN_ROAM_SCANS_IF_NO_CHANGE + zx::MonotonicDuration::from_seconds(1),
        ));

        // The limit on roams per day has been hit, no roam scan should be recommended.
        let should_roam_scan_result =
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone());
        assert_eq!(should_roam_scan_result, RoamTriggerDataOutcome::Noop);

        // Advance time to 24 hours past the first roam time. Roam scanning should now be allowed
        // again.
        exec.set_fake_time(
            first_roam_time
                + zx::MonotonicDuration::from_hours(24)
                + zx::MonotonicDuration::from_seconds(1),
        );
        let should_roam_scan_result =
            run_handle_roam_trigger_data(&mut exec, &mut test_values.monitor, trigger_data.clone());
        assert_variant!(should_roam_scan_result, RoamTriggerDataOutcome::RoamSearch { .. });
    }
}
