// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fidl::{CollaborativeRebootFromSvcDir, FidlServer, StateMachineController};
use crate::inspect::{LastResultsNode, ProtocolStateNode, ScheduleNode};
use crate::installer::{is_update_urgent, FuchsiaInstallError};
use anyhow::anyhow;
use fidl_fuchsia_feedback::{CrashReporterMarker, CrashReporterProxy};
use fuchsia_inspect::Node;
use futures::future::LocalBoxFuture;
use futures::prelude::*;
use log::{error, warn};
use omaha_client::clock;
use omaha_client::common::{ProtocolState, UpdateCheckSchedule};
use omaha_client::protocol::response::Response;
use omaha_client::state_machine::{
    update_check, InstallProgress, State, StateMachineEvent, UpdateCheckError,
};
use omaha_client::storage::Storage;
use omaha_client::time::{StandardTimeSource, TimeSource};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::SystemTime;

mod crash_report;
mod platform;

pub struct FuchsiaObserver<ST, SM>
where
    ST: Storage,
    SM: StateMachineController,
{
    fidl_server: Rc<RefCell<FidlServer<ST, SM>>>,
    schedule_node: ScheduleNode,
    protocol_state_node: ProtocolStateNode,
    last_results_node: LastResultsNode,
    last_update_start_time: SystemTime,
    target_version: Option<String>,
    platform_metrics_emitter: platform::Emitter,
    crash_reporter: Option<crash_report::CrashReportControlHandle>,
}

impl<ST, SM> FuchsiaObserver<ST, SM>
where
    ST: Storage + 'static,
    SM: StateMachineController,
{
    pub fn new(
        fidl_server: Rc<RefCell<FidlServer<ST, SM>>>,
        schedule_node: ScheduleNode,
        protocol_state_node: ProtocolStateNode,
        last_results_node: LastResultsNode,
        platform_metrics_node: Node,
    ) -> Self {
        FuchsiaObserver {
            fidl_server,
            schedule_node,
            protocol_state_node,
            last_results_node,
            last_update_start_time: SystemTime::UNIX_EPOCH,
            target_version: None,
            platform_metrics_emitter: platform::Emitter::from_node(platform_metrics_node),
            crash_reporter: None,
        }
    }

    pub fn start_handling_crash_reports(&mut self) -> LocalBoxFuture<'static, ()> {
        self.start_handling_crash_reports_impl(
            fuchsia_component::client::connect_to_protocol::<CrashReporterMarker>,
            StandardTimeSource,
        )
    }

    fn start_handling_crash_reports_impl<ProxyFn>(
        &mut self,
        proxy_fn: ProxyFn,
        time_source: impl TimeSource + 'static,
    ) -> LocalBoxFuture<'static, ()>
    where
        ProxyFn: FnOnce() -> Result<CrashReporterProxy, anyhow::Error>,
    {
        let proxy = match proxy_fn() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to connect to fuchsia.feedback/CrashReporter: {:#}", anyhow!(e));
                return future::ready(()).boxed_local();
            }
        };
        let (ch, fut) = crash_report::handle_crash_reports(proxy, time_source);
        self.crash_reporter = Some(ch);
        fut
    }

    pub async fn on_event(&mut self, event: StateMachineEvent) {
        match event {
            StateMachineEvent::StateChange(state) => self.on_state_change(state).await,
            StateMachineEvent::ScheduleChange(schedule) => self.on_schedule_change(&schedule),
            StateMachineEvent::ProtocolStateChange(state) => self.on_protocol_state_change(&state),
            StateMachineEvent::UpdateCheckResult(result) => {
                self.on_update_check_result(&result).await
            }
            StateMachineEvent::InstallProgressChange(progress) => {
                self.on_progress_change(progress).await
            }
            StateMachineEvent::OmahaServerResponse(response) => self.on_omaha_response(response),

            StateMachineEvent::InstallerError(e) => self.handle_installer_error(e),
        }
    }

    fn handle_installer_error(&mut self, e: Option<Box<dyn std::error::Error + Send + 'static>>) {
        if let Some(err) = e {
            // We only know how to handle Fuchsia Install errors, others will just be logged.
            let downcast_err = err.downcast::<FuchsiaInstallError>();
            if let Ok(fuchsia_install_error) = downcast_err {
                warn!("Got installer error: {:#}", anyhow!(fuchsia_install_error));
            } else {
                // This isn't a Fuchsia install error, and we don't know what it is, so we
                // don't know its size. Since it's unsized, it's not possible to wrap with
                // anyhow, so just log at [ERROR].
                error!("Got an unknown installer error: {:?}", downcast_err);
            }
        }

        if let Some(crash_reporter) = self.crash_reporter.as_mut() {
            if let Err(e) = crash_reporter.installation_error() {
                warn!("Failed to request installation error crash report: {:#}", anyhow!(e));
            }
        }
    }

    async fn on_state_change(&mut self, state: State) {
        match state {
            State::Idle => {
                self.target_version = None;
            }
            State::CheckingForUpdates(_) => {
                self.last_update_start_time = clock::now();
                self.platform_metrics_emitter.emit(platform::Event::CheckingForUpdates);
            }
            State::ErrorCheckingForUpdate => {
                self.platform_metrics_emitter.emit(platform::Event::ErrorCheckingForUpdate);
            }
            State::NoUpdateAvailable => {
                self.platform_metrics_emitter.emit(platform::Event::NoUpdateAvailable);
            }
            State::InstallingUpdate => {
                self.platform_metrics_emitter.emit(platform::Event::InstallingUpdate {
                    target_version: self.target_version.as_deref(),
                });
            }
            State::InstallationDeferredByPolicy => {
                self.platform_metrics_emitter.emit(platform::Event::InstallationDeferredByPolicy {
                    target_version: self.target_version.as_deref(),
                });
            }
            State::InstallationError => {
                self.platform_metrics_emitter.emit(platform::Event::InstallationError {
                    target_version: self.target_version.as_deref(),
                });
            }
            State::WaitingForReboot => {
                self.platform_metrics_emitter.emit(platform::Event::WaitingForReboot {
                    target_version: self.target_version.as_deref(),
                });
            }
        }
        FidlServer::on_state_change(
            Rc::clone(&self.fidl_server),
            state,
            &mut CollaborativeRebootFromSvcDir {},
        )
        .await
    }

    fn on_schedule_change(&mut self, schedule: &UpdateCheckSchedule) {
        self.schedule_node.set(schedule);
    }

    fn on_protocol_state_change(&mut self, protocol_state: &ProtocolState) {
        self.protocol_state_node.set(protocol_state);

        if let Some(crash_reporter) = self.crash_reporter.as_mut() {
            if let Err(e) = crash_reporter
                .consecutive_failed_update_checks(protocol_state.consecutive_failed_update_checks)
            {
                warn!(
                    "Failed to request consecutive failed update checks crash report: {:#}",
                    anyhow!(e)
                );
            }
        }
    }

    async fn on_update_check_result(
        &mut self,
        result: &Result<update_check::Response, UpdateCheckError>,
    ) {
        self.last_results_node.add_result(self.last_update_start_time, result);
    }

    async fn on_progress_change(&mut self, progress: InstallProgress) {
        FidlServer::on_progress_change(Rc::clone(&self.fidl_server), progress).await
    }

    fn on_omaha_response(&mut self, response: Response) {
        if let Some(update_check) =
            response.apps.into_iter().next().and_then(|app| app.update_check)
        {
            FidlServer::set_urgent_update(
                Rc::clone(&self.fidl_server),
                is_update_urgent(&update_check),
            );
            self.target_version = update_check.manifest.map(|manifest| manifest.version);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::crash_report::assert_signature;
    use super::*;
    use crate::fidl::{FidlServerBuilder, MockOrRealStateMachineController};
    use anyhow::anyhow;
    use assert_matches::assert_matches;
    use fidl_fuchsia_feedback::{CrashReport, FileReportResults};
    use fuchsia_async::{self as fasync, Task};
    use fuchsia_inspect::Inspector;
    use futures::channel::mpsc;
    use mock_crash_reporter::{MockCrashReporterService, ThrottleHook};
    use omaha_client::protocol::response::{self, Manifest, UpdateCheck};
    use omaha_client::storage::MemStorage;
    use omaha_client::time::MockTimeSource;
    use std::sync::Arc;
    use std::time::Duration;

    async fn new_test_observer() -> FuchsiaObserver<MemStorage, MockOrRealStateMachineController> {
        let fidl = FidlServerBuilder::new().build().await;
        let inspector = Inspector::default();
        let schedule_node = ScheduleNode::new(inspector.root().create_child("schedule"));
        let protocol_state_node =
            ProtocolStateNode::new(inspector.root().create_child("protocol_state"));
        let last_results_node = LastResultsNode::new(inspector.root().create_child("last_results"));
        let platform_metrics_node = inspector.root().create_child("platform_metrics");
        FuchsiaObserver::new(
            fidl,
            schedule_node,
            protocol_state_node,
            last_results_node,
            platform_metrics_node,
        )
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_cache_target_version() {
        let mut observer = new_test_observer().await;
        assert_eq!(observer.target_version, None);
        let response = Response {
            apps: vec![response::App {
                update_check: Some(UpdateCheck {
                    manifest: Some(Manifest {
                        version: "3.2.1".to_string(),
                        ..Manifest::default()
                    }),
                    ..UpdateCheck::default()
                }),
                ..response::App::default()
            }],
            ..Response::default()
        };
        observer.on_omaha_response(response);
        assert_eq!(observer.target_version, Some("3.2.1".to_string()));
        observer.on_state_change(State::Idle).await;
        assert_eq!(observer.target_version, None);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_cache_target_version_no_update() {
        let mut observer = new_test_observer().await;
        assert_eq!(observer.target_version, None);
        let response = Response {
            apps: vec![response::App {
                update_check: Some(UpdateCheck::no_update()),
                ..response::App::default()
            }],
            ..Response::default()
        };
        observer.on_omaha_response(response);
        assert_eq!(observer.target_version, None);
    }

    /// When we fail to get the CrashReporter proxy, the start_handling_crash_reports_impl
    /// future should immediately complete and we shouldn't set a control handle.
    #[fasync::run_singlethreaded(test)]
    async fn test_start_handling_crash_reports_proxyfn_error() {
        let mut observer = new_test_observer().await;

        let () = observer
            .start_handling_crash_reports_impl(|| Err(anyhow!("foo")), StandardTimeSource)
            .await;

        assert_matches!(observer.crash_reporter, None);
    }

    /// Verify we file crash reports on installation errors within a 24 hour band.
    #[fasync::run_singlethreaded(test)]
    async fn test_installation_error_crash_report() {
        let mut observer = new_test_observer().await;

        let (hook, mut recv) = ThrottleHook::new(Ok(FileReportResults::default()));
        let mock = Arc::new(MockCrashReporterService::new(hook));
        let (proxy, _fidl_server) = mock.spawn_crash_reporter_service();
        let mut time_source = MockTimeSource::new_from_now();
        let _handler = Task::local(
            observer.start_handling_crash_reports_impl(|| Ok(proxy), time_source.clone()),
        );

        observer.on_event(StateMachineEvent::InstallerError(None)).await;
        assert_signature(recv.next().await.unwrap(), "fuchsia-installation-error");

        // within 24 hrs so no report filed
        observer.on_event(StateMachineEvent::InstallerError(None)).await;
        assert_matches!(recv.try_next(), Err(_));

        // hit 24 hours so file report
        time_source.advance(Duration::from_secs(60 * 60 * 24));
        observer.on_event(StateMachineEvent::InstallerError(None)).await;
        assert_signature(recv.next().await.unwrap(), "fuchsia-installation-error");
    }

    async fn assert_files_consecutive_check_crash_report(
        observer: &mut FuchsiaObserver<MemStorage, MockOrRealStateMachineController>,
        n: u32,
        recv: &mut mpsc::Receiver<CrashReport>,
    ) {
        observer.on_protocol_state_change(&ProtocolState {
            consecutive_failed_update_checks: n,
            ..ProtocolState::default()
        });
        let signature = format!("fuchsia-{n}-consecutive-failed-update-checks");
        assert_signature(recv.next().await.unwrap(), &signature);
    }

    fn assert_does_not_file_consecutive_check_crash_report(
        observer: &mut FuchsiaObserver<MemStorage, MockOrRealStateMachineController>,
        n: u32,
        recv: &mut mpsc::Receiver<CrashReport>,
    ) {
        observer.on_protocol_state_change(&ProtocolState {
            consecutive_failed_update_checks: n,
            ..ProtocolState::default()
        });
        assert_matches!(recv.try_next(), Err(_));
    }

    /// Verify we file crash reports on >= 5 consecutive failed update checks.
    #[fasync::run_singlethreaded(test)]
    async fn test_consecutive_failed_update_checks_crash_report() {
        let mut observer = new_test_observer().await;

        let (hook, mut recv) = ThrottleHook::new(Ok(FileReportResults::default()));
        let mock = Arc::new(MockCrashReporterService::new(hook));
        let (proxy, _fidl_server) = mock.spawn_crash_reporter_service();
        let _handler = Task::local(
            observer.start_handling_crash_reports_impl(|| Ok(proxy), StandardTimeSource),
        );

        // Below 5 consecutive failed update checks, verify we DON'T file a crash report.
        assert_does_not_file_consecutive_check_crash_report(&mut observer, 1, &mut recv);
        assert_does_not_file_consecutive_check_crash_report(&mut observer, 2, &mut recv);
        assert_does_not_file_consecutive_check_crash_report(&mut observer, 3, &mut recv);
        assert_does_not_file_consecutive_check_crash_report(&mut observer, 4, &mut recv);

        // >=5 consecutive failed update checks, verify we DO file a crash report on a backoff.
        assert_files_consecutive_check_crash_report(&mut observer, 5, &mut recv).await;

        assert_files_consecutive_check_crash_report(&mut observer, 6, &mut recv).await;
        assert_does_not_file_consecutive_check_crash_report(&mut observer, 7, &mut recv);

        assert_files_consecutive_check_crash_report(&mut observer, 8, &mut recv).await;
        assert_does_not_file_consecutive_check_crash_report(&mut observer, 9, &mut recv);
        assert_does_not_file_consecutive_check_crash_report(&mut observer, 10, &mut recv);
        assert_does_not_file_consecutive_check_crash_report(&mut observer, 11, &mut recv);

        assert_files_consecutive_check_crash_report(&mut observer, 12, &mut recv).await;
    }
}
