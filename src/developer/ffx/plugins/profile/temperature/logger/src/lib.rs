// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use async_trait::async_trait;
use errors::ffx_bail;
use ffx_temperature_logger_args as args_mod;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool};
use fidl_fuchsia_power_metrics::{self as fmetrics, Metric, StatisticsArgs, Temperature};
use target_holders::moniker;

#[derive(FfxTool)]
pub struct TemperatureLoggerTool {
    #[command]
    cmd: args_mod::Command,
    #[with(moniker("/core/metrics-logger"))]
    temperature_logger: fmetrics::RecorderProxy,
}

fho::embedded_plugin!(TemperatureLoggerTool);

#[async_trait(?Send)]
impl FfxMain for TemperatureLoggerTool {
    type Writer = SimpleWriter;
    async fn main(self, _writer: Self::Writer) -> fho::Result<()> {
        match self.cmd.subcommand {
            args_mod::SubCommand::Start(start_cmd) => {
                start(self.temperature_logger, start_cmd).await?
            }
            args_mod::SubCommand::Stop(_) => stop(self.temperature_logger).await?,
        };
        Ok(())
    }
}

pub async fn start(
    temperature_logger: fmetrics::RecorderProxy,
    cmd: args_mod::StartCommand,
) -> Result<()> {
    let statistics_args = cmd
        .statistics_interval
        .map(|i| Box::new(StatisticsArgs { statistics_interval_ms: i.as_millis() as u32 }));
    let sampling_interval_ms = cmd.sampling_interval.as_millis() as u32;

    // Dispatch to Recorder.StartLogging or Recorder.StartLoggingForever,
    // depending on whether a logging duration is specified.
    let result = if let Some(duration) = cmd.duration {
        let duration_ms = duration.as_millis() as u32;
        temperature_logger
            .start_logging(
                "ffx_temperature",
                &[Metric::Temperature(Temperature { sampling_interval_ms, statistics_args })],
                duration_ms,
                cmd.output_samples_to_syslog,
                cmd.output_stats_to_syslog,
            )
            .await?
    } else {
        temperature_logger
            .start_logging_forever(
                "ffx_temperature",
                &[Metric::Temperature(Temperature { sampling_interval_ms, statistics_args })],
                cmd.output_samples_to_syslog,
                cmd.output_stats_to_syslog,
            )
            .await?
    };

    match result {
        Err(fmetrics::RecorderError::InvalidSamplingInterval) => ffx_bail!(
            "Recorder.StartLogging received an invalid sampling interval. \n\
            Please check if `sampling-interval` meets the following requirements: \n\
            1) Must be smaller than `duration` if `duration` is specified; \n\
            2) Must not be smaller than 500ms if `output_samples_to_syslog` is enabled."
        ),
        Err(fmetrics::RecorderError::AlreadyLogging) => ffx_bail!(
            "Ffx temperature logging is already active. Use \"stop\" subcommand to stop the active \
            loggingg manually."
        ),
        Err(fmetrics::RecorderError::NoDrivers) => {
            ffx_bail!("This device has no sensor for logging temperature.")
        }
        Err(fmetrics::RecorderError::TooManyActiveClients) => ffx_bail!(
            "Recorder is running too many clients. Retry after any other client is stopped."
        ),
        Err(fmetrics::RecorderError::InvalidStatisticsInterval) => ffx_bail!(
            "Recorder.StartLogging received an invalid statistics interval. \n\
            Please check if `statistics-interval` meets the following requirements: \n\
            1) Must be equal to or larger than `sampling-interval`; \n\
            2) Must be smaller than `duration` if `duration` is specified; \n\
            3) Must not be smaller than 500ms if `output_stats_to_syslog` is enabled."
        ),
        _ => Ok(()),
    }
}

pub async fn stop(temperature_logger: fmetrics::RecorderProxy) -> Result<()> {
    if !temperature_logger.stop_logging("ffx_temperature").await? {
        ffx_bail!("Stop logging returned false; Check if logging is already inactive.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl_fuchsia_power_metrics::{self as fmetrics};
    use futures::channel::mpsc;
    use std::time::Duration;
    use target_holders::fake_proxy;

    // Create a metrics-logger that expects a specific request type (Start, StartForever, or
    // Stop), and returns a specific error
    macro_rules! make_logger {
        ($request_type:tt, $error_type:tt) => {
            fake_proxy(move |req| match req {
                fmetrics::RecorderRequest::$request_type { responder, .. } => {
                    responder.send(Err(fmetrics::RecorderError::$error_type)).unwrap();
                }
                _ => {
                    panic!("Expected RecorderRequest::{}; got {:?}", stringify!($request_type), req)
                }
            })
        };
    }

    const ONE_SEC: Duration = Duration::from_secs(1);

    /// Confirms that the start logging request is dispatched to FIDL requests as expected.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_request_dispatch_start_logging() {
        // Start logging: sampling_interval=1s, statistics_interval=2s, duration=4s
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(2 * ONE_SEC),
            duration: Some(4 * ONE_SEC),
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let (mut sender, mut receiver) = mpsc::channel(1);
        let logger = fake_proxy(move |req| match req {
            fmetrics::RecorderRequest::StartLogging {
                client_id,
                metrics,
                duration_ms,
                output_samples_to_syslog,
                output_stats_to_syslog,
                responder,
            } => {
                assert_eq!(String::from("ffx_temperature"), client_id);
                assert_eq!(metrics.len(), 1);
                assert_eq!(
                    metrics[0],
                    Metric::Temperature(Temperature {
                        sampling_interval_ms: 1000,
                        statistics_args: Some(Box::new(StatisticsArgs {
                            statistics_interval_ms: 2000
                        })),
                    }),
                );
                assert_eq!(output_samples_to_syslog, false);
                assert_eq!(output_stats_to_syslog, false);
                assert_eq!(duration_ms, 4000);
                responder.send(Ok(())).unwrap();
                sender.try_send(()).unwrap();
            }
            _ => panic!("Expected RecorderRequest::StartLogging; got {:?}", req),
        });
        start(logger, args).await.unwrap();
        assert_matches!(receiver.try_next().unwrap(), Some(()));
    }

    /// Confirms that the start logging forever request is dispatched to FIDL requests as expected.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_request_dispatch_start_logging_forever() {
        // Start logging: sampling_interval=1s, statistics_interval=2s, duration=forever
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(2 * ONE_SEC),
            duration: None,
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let (mut sender, mut receiver) = mpsc::channel(1);
        let logger = fake_proxy(move |req| match req {
            fmetrics::RecorderRequest::StartLoggingForever {
                client_id,
                metrics,
                output_samples_to_syslog,
                output_stats_to_syslog,
                responder,
                ..
            } => {
                assert_eq!(String::from("ffx_temperature"), client_id);
                assert_eq!(metrics.len(), 1);
                assert_eq!(
                    metrics[0],
                    Metric::Temperature(Temperature {
                        sampling_interval_ms: 1000,
                        statistics_args: Some(Box::new(StatisticsArgs {
                            statistics_interval_ms: 2000
                        })),
                    }),
                );
                assert_eq!(output_samples_to_syslog, false);
                assert_eq!(output_stats_to_syslog, false);
                responder.send(Ok(())).unwrap();
                sender.try_send(()).unwrap();
            }
            _ => panic!("Expected RecorderRequest::StartLoggingForever; got {:?}", req),
        });
        start(logger, args).await.unwrap();
        assert_matches!(receiver.try_next().unwrap(), Some(()));
    }

    /// Confirms that the stop logging request is dispatched to FIDL requests as expected.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_request_dispatch_stop_logging() {
        // Stop logging
        let (mut sender, mut receiver) = mpsc::channel(1);
        let logger = fake_proxy(move |req| match req {
            fmetrics::RecorderRequest::StopLogging { client_id, responder } => {
                assert_eq!(String::from("ffx_temperature"), client_id);
                responder.send(true).unwrap();
                sender.try_send(()).unwrap();
            }
            _ => panic!("Expected RecorderRequest::StopLogging; got {:?}", req),
        });
        stop(logger).await.unwrap();
        assert_matches!(receiver.try_next().unwrap(), Some(()));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_stop_logging_error() {
        let logger = fake_proxy(move |req| match req {
            fmetrics::RecorderRequest::StopLogging { responder, .. } => {
                responder.send(false).unwrap();
            }
            _ => panic!("Expected RecorderRequest::StopLogging; got {:?}", req),
        });
        let error = stop(logger).await.unwrap_err();
        assert!(error.to_string().contains("Stop logging returned false"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_samplingg_interval_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: Some(2 * ONE_SEC),
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLogging, InvalidSamplingInterval);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("invalid sampling interval"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_forever_sampling_interval_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: None,
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLoggingForever, InvalidSamplingInterval);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("invalid sampling interval"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_statistics_interval_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: Some(2 * ONE_SEC),
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLogging, InvalidStatisticsInterval);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("invalid statistics interval"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_forever_statistics_interval_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: None,
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLoggingForever, InvalidStatisticsInterval);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("invalid statistics interval"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_already_active_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: Some(2 * ONE_SEC),
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLogging, AlreadyLogging);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("already active"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_forever_already_active_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: None,
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLoggingForever, AlreadyLogging);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("already active"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_too_many_clients_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: Some(2 * ONE_SEC),
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLogging, TooManyActiveClients);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("too many clients"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_forever_too_many_clients_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: None,
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLoggingForever, TooManyActiveClients);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("too many clients"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_no_sensor_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: Some(2 * ONE_SEC),
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLogging, NoDrivers);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("no sensor"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_start_logging_forever_no_sensor_error() {
        let args = args_mod::StartCommand {
            sampling_interval: ONE_SEC,
            statistics_interval: Some(ONE_SEC),
            duration: None,
            output_samples_to_syslog: false,
            output_stats_to_syslog: false,
        };
        let logger = make_logger!(StartLoggingForever, NoDrivers);
        let error = start(logger, args).await.unwrap_err();
        assert!(error.to_string().contains("no sensor"));
    }
}
