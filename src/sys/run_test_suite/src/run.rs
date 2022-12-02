// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        artifacts,
        cancel::{Cancelled, NamedFutureExt, OrCancel},
        diagnostics::{self, LogDisplayConfiguration},
        outcome::{Lifecycle, Outcome, RunTestSuiteError, UnexpectedEventError},
        output::{self, ArtifactType, CaseId, RunReporter, SuiteId, SuiteReporter, Timestamp},
        params::{RunParams, TestParams, TimeoutBehavior},
        stream_util::StreamUtil,
        trace::duration,
    },
    diagnostics_data::Severity,
    fidl_fuchsia_test_manager::{
        self as ftest_manager, CaseArtifact, CaseFinished, CaseFound, CaseStarted, CaseStopped,
        RunBuilderProxy, SuiteArtifact, SuiteStopped,
    },
    fuchsia_async as fasync,
    futures::future::Either,
    futures::{prelude::*, stream::FuturesUnordered, StreamExt},
    std::collections::HashMap,
    std::io::Write,
    std::path::PathBuf,
    std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    tracing::{error, info, warn},
};

/// Struct used by |run_suite_and_collect_logs| to track the state of test cases and suites.
struct CollectedEntityState<R> {
    reporter: R,
    name: String,
    lifecycle: Lifecycle,
    artifact_tasks:
        Vec<fasync::Task<Result<Option<diagnostics::LogCollectionOutcome>, anyhow::Error>>>,
}

/// Collects results and artifacts for a single suite.
// TODO(satsukiu): There's two ways to return an error here:
// * Err(RunTestSuiteError)
// * Ok(Outcome::Error(RunTestSuiteError))
// We should consider how to consolidate these.
async fn run_suite_and_collect_logs<F: Future<Output = ()> + Unpin>(
    running_suite: RunningSuite,
    suite_reporter: &SuiteReporter<'_>,
    log_display: diagnostics::LogDisplayConfiguration,
    cancel_fut: F,
) -> Result<Outcome, RunTestSuiteError> {
    duration!("collect_suite");

    let RunningSuite {
        mut event_stream, stopper, timeout, timeout_grace, max_severity_logs, ..
    } = running_suite;

    let log_opts =
        diagnostics::LogCollectionOptions { format: log_display, max_severity: max_severity_logs };

    let mut test_cases: HashMap<u32, CollectedEntityState<_>> = HashMap::new();
    let mut suite_state = CollectedEntityState {
        reporter: suite_reporter,
        name: "".to_string(),
        lifecycle: Lifecycle::Found,
        artifact_tasks: vec![],
    };
    let mut suite_finish_timestamp = Timestamp::Unknown;
    let mut outcome = Outcome::Passed;

    let collect_results_fut = async {
        while let Some(event_result) = event_stream.next().named("next_event").await {
            match event_result {
                Err(e) => {
                    suite_state
                        .reporter
                        .stopped(&output::ReportedOutcome::Error, Timestamp::Unknown)
                        .await?;
                    return Err(e);
                }
                Ok(event) => {
                    let timestamp = Timestamp::from_nanos(event.timestamp);
                    match event.payload.expect("event cannot be None") {
                        ftest_manager::SuiteEventPayload::CaseFound(CaseFound {
                            test_case_name,
                            identifier,
                        }) => {
                            if test_cases.contains_key(&identifier) {
                                return Err(UnexpectedEventError::InvalidCaseEvent {
                                    last_state: Lifecycle::Found,
                                    next_state: Lifecycle::Found,
                                    test_case_name,
                                    identifier,
                                }
                                .into());
                            }
                            test_cases.insert(
                                identifier,
                                CollectedEntityState {
                                    reporter: suite_reporter
                                        .new_case(&test_case_name, &CaseId(identifier))
                                        .await?,
                                    name: test_case_name,
                                    lifecycle: Lifecycle::Found,
                                    artifact_tasks: vec![],
                                },
                            );
                        }
                        ftest_manager::SuiteEventPayload::CaseStarted(CaseStarted {
                            identifier,
                        }) => {
                            let entry = test_cases.get_mut(&identifier).ok_or(
                                UnexpectedEventError::CaseEventButNotFound {
                                    next_state: Lifecycle::Started,
                                    identifier,
                                },
                            )?;
                            match &entry.lifecycle {
                                Lifecycle::Found => {
                                    // TODO(fxbug.dev/79712): Record per-case runtime once we have an
                                    // accurate way to measure it.
                                    entry.reporter.started(Timestamp::Unknown).await?;
                                    entry.lifecycle = Lifecycle::Started;
                                }
                                other => {
                                    return Err(UnexpectedEventError::InvalidCaseEvent {
                                        last_state: *other,
                                        next_state: Lifecycle::Started,
                                        test_case_name: entry.name.clone(),
                                        identifier,
                                    }
                                    .into());
                                }
                            }
                        }
                        ftest_manager::SuiteEventPayload::CaseArtifact(CaseArtifact {
                            identifier,
                            artifact,
                        }) => {
                            let entry = test_cases.get_mut(&identifier).ok_or(
                                UnexpectedEventError::CaseArtifactButNotFound { identifier },
                            )?;
                            if matches!(entry.lifecycle, Lifecycle::Finished) {
                                return Err(UnexpectedEventError::CaseArtifactButFinished {
                                    identifier,
                                }
                                .into());
                            }
                            let artifact_fut = artifacts::drain_artifact(
                                &entry.reporter,
                                artifact,
                                log_opts.clone(),
                            )
                            .await?;
                            entry.artifact_tasks.push(fasync::Task::spawn(artifact_fut));
                        }
                        ftest_manager::SuiteEventPayload::CaseStopped(CaseStopped {
                            identifier,
                            status,
                        }) => {
                            let entry = test_cases.get_mut(&identifier).ok_or(
                                UnexpectedEventError::CaseEventButNotFound {
                                    next_state: Lifecycle::Stopped,
                                    identifier,
                                },
                            )?;
                            match &entry.lifecycle {
                                Lifecycle::Started => {
                                    // TODO(fxbug.dev/79712): Record per-case runtime once we have an
                                    // accurate way to measure it.
                                    entry
                                        .reporter
                                        .stopped(&status.into(), Timestamp::Unknown)
                                        .await?;
                                    entry.lifecycle = Lifecycle::Stopped;
                                }
                                other => {
                                    return Err(UnexpectedEventError::InvalidCaseEvent {
                                        last_state: *other,
                                        next_state: Lifecycle::Stopped,
                                        test_case_name: entry.name.clone(),
                                        identifier,
                                    }
                                    .into());
                                }
                            }
                        }
                        ftest_manager::SuiteEventPayload::CaseFinished(CaseFinished {
                            identifier,
                        }) => {
                            let entry = test_cases.get_mut(&identifier).ok_or(
                                UnexpectedEventError::CaseEventButNotFound {
                                    next_state: Lifecycle::Finished,
                                    identifier,
                                },
                            )?;
                            match &entry.lifecycle {
                                Lifecycle::Stopped => {
                                    // don't mark reporter finished yet, we want to finish draining
                                    // artifacts separately.
                                    entry.lifecycle = Lifecycle::Finished;
                                }
                                other => {
                                    return Err(UnexpectedEventError::InvalidCaseEvent {
                                        last_state: *other,
                                        next_state: Lifecycle::Finished,
                                        test_case_name: entry.name.clone(),
                                        identifier,
                                    }
                                    .into());
                                }
                            }
                        }
                        ftest_manager::SuiteEventPayload::SuiteArtifact(SuiteArtifact {
                            artifact,
                        }) => {
                            let artifact_fut = artifacts::drain_artifact(
                                suite_reporter,
                                artifact,
                                log_opts.clone(),
                            )
                            .await?;
                            suite_state.artifact_tasks.push(fasync::Task::spawn(artifact_fut));
                        }
                        ftest_manager::SuiteEventPayload::SuiteStarted(_) => {
                            match &suite_state.lifecycle {
                                Lifecycle::Found => {
                                    suite_state.reporter.started(timestamp).await?;
                                    suite_state.lifecycle = Lifecycle::Started;
                                }
                                other => {
                                    return Err(UnexpectedEventError::InvalidSuiteEvent {
                                        last_state: *other,
                                        next_state: Lifecycle::Started,
                                    }
                                    .into());
                                }
                            }
                        }
                        ftest_manager::SuiteEventPayload::SuiteStopped(SuiteStopped { status }) => {
                            match &suite_state.lifecycle {
                                Lifecycle::Started => {
                                    suite_state.lifecycle = Lifecycle::Stopped;
                                    suite_finish_timestamp = timestamp;
                                    outcome = match status {
                                        ftest_manager::SuiteStatus::Passed => Outcome::Passed,
                                        ftest_manager::SuiteStatus::Failed => Outcome::Failed,
                                        ftest_manager::SuiteStatus::DidNotFinish => {
                                            Outcome::Inconclusive
                                        }
                                        ftest_manager::SuiteStatus::TimedOut => Outcome::Timedout,
                                        ftest_manager::SuiteStatus::Stopped => Outcome::Failed,
                                        ftest_manager::SuiteStatus::InternalError => {
                                            Outcome::error(
                                                UnexpectedEventError::InternalErrorSuiteStatus,
                                            )
                                        }
                                        s => {
                                            return Err(
                                                UnexpectedEventError::UnrecognizedSuiteStatus {
                                                    status: s,
                                                }
                                                .into(),
                                            );
                                        }
                                    };
                                }
                                other => {
                                    return Err(UnexpectedEventError::InvalidSuiteEvent {
                                        last_state: *other,
                                        next_state: Lifecycle::Stopped,
                                    }
                                    .into());
                                }
                            }
                        }
                        ftest_manager::SuiteEventPayloadUnknown!() => {
                            warn!("Encountered unrecognized suite event");
                        }
                    }
                }
            }
        }
        drop(event_stream); // Explicit drop here to force ownership move.
        Ok(())
    }
    .boxed_local();

    let start_time = std::time::Instant::now();
    let (stop_timeout_future, kill_timeout_future) = match timeout {
        None => {
            (futures::future::pending::<()>().boxed(), futures::future::pending::<()>().boxed())
        }
        Some(duration) => (
            fasync::Timer::new(start_time + duration).boxed(),
            fasync::Timer::new(start_time + duration + timeout_grace).boxed(),
        ),
    };

    // This polls event collection and calling SuiteController::Stop on timeout simultaneously.
    let collect_or_stop_fut = async move {
        match futures::future::select(stop_timeout_future, collect_results_fut).await {
            Either::Left((_stop_done, collect_fut)) => {
                stopper.stop();
                collect_fut.await
            }
            Either::Right((result, _)) => result,
        }
    };

    // If kill timeout or cancel occur, we want to stop polling events.
    // kill_fut resolves to the outcome to which results should be overwritten
    // if it resolves.
    let kill_fut = async move {
        match futures::future::select(cancel_fut, kill_timeout_future).await {
            Either::Left(_) => Outcome::Cancelled,
            Either::Right(_) => Outcome::Timedout,
        }
    }
    .shared();

    let early_termination_outcome =
        match collect_or_stop_fut.boxed_local().or_cancelled(kill_fut.clone()).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => return Err(e),
            Err(Cancelled(outcome)) => Some(outcome),
        };

    // Finish collecting artifacts and report errors.
    info!("Awaiting case artifacts");
    let mut unfinished_test_case_names = vec![];
    for (_, test_case) in test_cases.into_iter() {
        let CollectedEntityState { reporter, name, lifecycle, artifact_tasks } = test_case;
        match (lifecycle, early_termination_outcome.clone()) {
            (Lifecycle::Started | Lifecycle::Found, Some(early)) => {
                reporter.stopped(&early.into(), Timestamp::Unknown).await?;
            }
            (Lifecycle::Found, None) => {
                unfinished_test_case_names.push(name.clone());
                reporter.stopped(&Outcome::Inconclusive.into(), Timestamp::Unknown).await?;
            }
            (Lifecycle::Started, None) => {
                unfinished_test_case_names.push(name.clone());
                reporter.stopped(&Outcome::DidNotFinish.into(), Timestamp::Unknown).await?;
            }
            (Lifecycle::Stopped | Lifecycle::Finished, _) => (),
        }

        let finish_artifacts_fut = FuturesUnordered::from_iter(artifact_tasks)
            .map(|result| match result {
                Err(e) => {
                    error!("Failed to collect artifact for {}: {:?}", name, e);
                }
                Ok(Some(_log_result)) => warn!("Unexpectedly got log results for a test case"),
                Ok(None) => (),
            })
            .collect::<()>();
        if let Err(Cancelled(_)) = finish_artifacts_fut.or_cancelled(kill_fut.clone()).await {
            warn!("Stopped polling artifacts for {} due to timeout", name);
        }

        reporter.finished().await?;
    }
    if !unfinished_test_case_names.is_empty() {
        outcome = Outcome::error(UnexpectedEventError::CasesDidNotFinish {
            cases: unfinished_test_case_names,
        });
    }

    match (suite_state.lifecycle, early_termination_outcome) {
        (Lifecycle::Found | Lifecycle::Started, Some(early)) => {
            if matches!(&outcome, Outcome::Passed | Outcome::Failed) {
                outcome = early;
            }
        }
        (Lifecycle::Found | Lifecycle::Started, None) => {
            outcome = Outcome::error(UnexpectedEventError::SuiteDidNotReportStop);
        }
        // If the suite successfully reported a result, don't alter it.
        (Lifecycle::Stopped, _) => (),
        // Finished doesn't happen since there's no SuiteFinished event.
        (Lifecycle::Finished, _) => unreachable!(),
    }

    let restricted_logs_present = AtomicBool::new(false);
    let finish_artifacts_fut = FuturesUnordered::from_iter(suite_state.artifact_tasks)
        .then(|result| async {
            match result {
                Err(e) => {
                    error!("Failed to collect artifact for suite: {:?}", e);
                }
                Ok(Some(log_result)) => match log_result {
                    diagnostics::LogCollectionOutcome::Error { restricted_logs } => {
                        restricted_logs_present.store(true, Ordering::Relaxed);
                        let mut log_artifact = match suite_reporter
                            .new_artifact(&ArtifactType::RestrictedLog)
                            .await
                        {
                            Ok(artifact) => artifact,
                            Err(e) => {
                                warn!("Error creating artifact to report restricted logs: {:?}", e);
                                return;
                            }
                        };
                        for log in restricted_logs.iter() {
                            if let Err(e) = writeln!(log_artifact, "{}", log) {
                                warn!("Error recording restricted logs: {:?}", e);
                                return;
                            }
                        }
                    }
                    diagnostics::LogCollectionOutcome::Passed => (),
                },
                Ok(None) => (),
            }
        })
        .collect::<()>();
    if let Err(Cancelled(_)) = finish_artifacts_fut.or_cancelled(kill_fut).await {
        warn!("Stopped polling artifacts due to timeout");
    }
    if restricted_logs_present.into_inner() && matches!(outcome, Outcome::Passed) {
        outcome = Outcome::Failed;
    }

    suite_reporter.stopped(&outcome.clone().into(), suite_finish_timestamp).await?;

    Ok(outcome)
}

type SuiteEventStream = std::pin::Pin<
    Box<dyn Stream<Item = Result<ftest_manager::SuiteEvent, RunTestSuiteError>> + Send>,
>;

/// A test suite that is known to have started execution. A suite is considered started once
/// any event is produced for the suite.
struct RunningSuite {
    event_stream: SuiteEventStream,
    stopper: RunningSuiteStopper,
    max_severity_logs: Option<Severity>,
    timeout: Option<std::time::Duration>,
    timeout_grace: std::time::Duration,
}

struct RunningSuiteStopper(Arc<ftest_manager::SuiteControllerProxy>);

impl RunningSuiteStopper {
    fn stop(self) {
        let _ = self.0.stop();
    }
}

impl RunningSuite {
    /// Number of concurrently active GetEvents requests. Chosen by testing powers of 2 when
    /// running a set of tests using ffx test against an emulator, and taking the value at
    /// which improvement stops.
    const DEFAULT_PIPELINED_REQUESTS: usize = 8;
    async fn wait_for_start(
        proxy: ftest_manager::SuiteControllerProxy,
        max_severity_logs: Option<Severity>,
        timeout: Option<std::time::Duration>,
        timeout_grace: std::time::Duration,
        max_pipelined: Option<usize>,
    ) -> Self {
        let proxy = Arc::new(proxy);
        let proxy_clone = proxy.clone();
        // Stream of fidl responses, with multiple concurrently active requests.
        let unprocessed_event_stream = futures::stream::repeat_with(move || {
            proxy.get_events().inspect(|events_result| match events_result {
                Ok(Ok(ref events)) => info!("Latest suite event: {:?}", events.last()),
                _ => (),
            })
        })
        .buffered(max_pipelined.unwrap_or(Self::DEFAULT_PIPELINED_REQUESTS));
        // Terminate the stream after we get an error or empty list of events.
        let terminated_event_stream =
            unprocessed_event_stream.take_until_stop_after(|result| match &result {
                Ok(Ok(events)) => events.is_empty(),
                Err(_) | Ok(Err(_)) => true,
            });
        // Flatten the stream of vecs into a stream of single events.
        let mut event_stream = terminated_event_stream
            .map(Self::convert_to_result_vec)
            .map(futures::stream::iter)
            .flatten()
            .peekable();
        // Wait for the first event to be ready, which signals the suite has started.
        std::pin::Pin::new(&mut event_stream).peek().await;

        Self {
            event_stream: event_stream.boxed(),
            stopper: RunningSuiteStopper(proxy_clone),
            timeout,
            timeout_grace,
            max_severity_logs,
        }
    }

    fn convert_to_result_vec(
        vec: Result<
            Result<Vec<ftest_manager::SuiteEvent>, ftest_manager::LaunchError>,
            fidl::Error,
        >,
    ) -> Vec<Result<ftest_manager::SuiteEvent, RunTestSuiteError>> {
        match vec {
            Ok(Ok(events)) => events.into_iter().map(Ok).collect(),
            Ok(Err(e)) => vec![Err(e.into())],
            Err(e) => vec![Err(e.into())],
        }
    }
}

// Will invoke the WithSchedulingOptions FIDL method if a client indicates
// that they want to use experimental parallel execution.
async fn request_scheduling_options(
    run_params: &RunParams,
    builder_proxy: &RunBuilderProxy,
) -> Result<(), RunTestSuiteError> {
    let scheduling_options = ftest_manager::SchedulingOptions {
        max_parallel_suites: run_params.experimental_parallel_execution,
        accumulate_debug_data: Some(run_params.accumulate_debug_data),
        ..ftest_manager::SchedulingOptions::EMPTY
    };
    builder_proxy.with_scheduling_options(scheduling_options)?;
    Ok(())
}

/// Schedule and run the tests specified in |test_params|, and collect the results.
/// Note this currently doesn't record the result or call finished() on run_reporter,
/// the caller should do this instead.
async fn run_tests<'a, F: 'a + Future<Output = ()> + Unpin>(
    builder_proxy: RunBuilderProxy,
    test_params: impl IntoIterator<Item = TestParams>,
    run_params: RunParams,
    run_reporter: &'a RunReporter,
    cancel_fut: F,
) -> Result<Outcome, RunTestSuiteError> {
    let mut suite_start_futs = FuturesUnordered::new();
    let mut suite_reporters = HashMap::new();
    for (suite_id_raw, params) in test_params.into_iter().enumerate() {
        let timeout = params
            .timeout_seconds
            .map(|seconds| std::time::Duration::from_secs(seconds.get() as u64));

        let run_options = fidl_fuchsia_test_manager::RunOptions {
            parallel: params.parallel,
            arguments: Some(params.test_args),
            run_disabled_tests: Some(params.also_run_disabled_tests),
            case_filters_to_run: params.test_filters,
            log_iterator: Some(run_params.log_protocol.unwrap_or_else(diagnostics::get_type)),
            ..fidl_fuchsia_test_manager::RunOptions::EMPTY
        };

        let suite_id = SuiteId(suite_id_raw as u32);
        let suite = run_reporter.new_suite(&params.test_url, &suite_id).await?;
        suite.set_tags(params.tags).await;
        suite_reporters.insert(suite_id, suite);
        let (suite_controller, suite_server_end) = fidl::endpoints::create_proxy()?;
        let suite_and_id_fut = RunningSuite::wait_for_start(
            suite_controller,
            params.max_severity_logs,
            timeout,
            std::time::Duration::from_secs(run_params.timeout_grace_seconds as u64),
            None,
        )
        .map(move |running_suite| (running_suite, suite_id));
        suite_start_futs.push(suite_and_id_fut);
        builder_proxy.add_suite(&params.test_url, run_options, suite_server_end)?;
    }

    request_scheduling_options(&run_params, &builder_proxy).await?;
    let (run_controller, run_server_end) = fidl::endpoints::create_proxy()?;
    let run_controller_ref = &run_controller;
    builder_proxy.build(run_server_end)?;
    run_reporter.started(Timestamp::Unknown).await?;
    let cancel_fut = cancel_fut.shared();
    let cancel_fut_clone = cancel_fut.clone();

    let handle_suite_fut = async move {
        let mut num_failed = 0;
        let mut final_outcome = None;
        let mut stopped_prematurely = false;
        // for now, we assume that suites are run serially.
        loop {
            let (running_suite, suite_id) = match suite_start_futs
                .next()
                .named("suite_start")
                .or_cancelled(cancel_fut.clone())
                .await
            {
                Ok(Some((running_suite, suite_id))) => (running_suite, suite_id),
                // normal completion.
                Ok(None) => break,
                Err(Cancelled(_)) => {
                    stopped_prematurely = true;
                    final_outcome = Some(Outcome::Cancelled);
                    break;
                }
            };

            let suite_reporter = suite_reporters.remove(&suite_id).unwrap();

            let log_display = LogDisplayConfiguration {
                show_full_moniker: run_params.show_full_moniker,
                min_severity: run_params.min_severity_logs,
            };

            let result = run_suite_and_collect_logs(
                running_suite,
                &suite_reporter,
                log_display,
                cancel_fut.clone(),
            )
            .await;
            let suite_outcome = result.unwrap_or_else(|err| Outcome::error(err));
            // We should always persist results, even if something failed.
            suite_reporter.finished().await?;

            num_failed = match suite_outcome {
                Outcome::Passed => num_failed,
                _ => num_failed + 1,
            };
            let stop_due_to_timeout = match run_params.timeout_behavior {
                TimeoutBehavior::TerminateRemaining => suite_outcome == Outcome::Timedout,
                TimeoutBehavior::Continue => false,
            };
            let stop_due_to_failures = match run_params.stop_after_failures.as_ref() {
                Some(threshold) => num_failed >= threshold.get(),
                None => false,
            };
            let stop_due_to_cancellation = matches!(&suite_outcome, Outcome::Cancelled);
            let stop_due_to_internal_error = match &suite_outcome {
                Outcome::Error { origin } => origin.is_internal_error(),
                _ => false,
            };

            final_outcome = match (final_outcome.take(), suite_outcome) {
                (None, first_outcome) => Some(first_outcome),
                (Some(outcome), Outcome::Passed) => Some(outcome),
                (Some(_), failing_outcome) => Some(failing_outcome),
            };
            if stop_due_to_timeout
                || stop_due_to_failures
                || stop_due_to_cancellation
                || stop_due_to_internal_error
            {
                stopped_prematurely = true;
                break;
            }
        }
        if stopped_prematurely {
            // Ignore errors here since we're stopping anyway.
            let _ = run_controller_ref.stop();
            // Drop remaining controllers, which is the same as calling kill on
            // each controller.
            suite_start_futs.clear();
            for (_id, reporter) in suite_reporters.drain() {
                reporter.finished().await?;
            }
        }
        Ok(final_outcome.unwrap_or(Outcome::Passed))
    };

    let handle_run_events_fut = async move {
        duration!("run_events");
        let mut artifact_tasks = vec![];
        loop {
            let events = run_controller_ref.get_events().named("run_event").await?;
            if events.len() == 0 {
                break;
            }

            for event in events.into_iter() {
                let ftest_manager::RunEvent { payload, .. } = event;
                match payload {
                    // TODO(fxbug.dev/91151): Add support for RunStarted and RunStopped when test_manager sends them.
                    Some(ftest_manager::RunEventPayload::Artifact(artifact)) => {
                        let artifact_fut = artifacts::drain_artifact(
                            run_reporter,
                            artifact,
                            diagnostics::LogCollectionOptions {
                                max_severity: None,
                                format: LogDisplayConfiguration {
                                    show_full_moniker: run_params.show_full_moniker,
                                    min_severity: run_params.min_severity_logs,
                                },
                            },
                        )
                        .await?;
                        artifact_tasks.push(fasync::Task::spawn(artifact_fut));
                    }
                    e => {
                        warn!("Discarding run event: {:?}", e);
                    }
                }
            }
        }
        for task in artifact_tasks {
            match task.await {
                Err(e) => {
                    error!("Failed to collect artifact for run: {:?}", e);
                }
                Ok(Some(_log_result)) => warn!("Unexpectedly got log results for the test run"),
                Ok(None) => (),
            }
        }
        Ok(())
    };

    // Make sure we stop polling run events on cancel. Since cancellation is expected
    // ignore cancellation errors.
    let cancellable_run_events_fut = handle_run_events_fut
        .boxed_local()
        .or_cancelled(cancel_fut_clone)
        .map(|cancelled_result| match cancelled_result {
            Ok(completed_result) => completed_result,
            Err(Cancelled(_)) => Ok(()),
        });

    // Use join instead of try_join as we want to poll the futures to completion
    // even if one fails.
    match futures::future::join(handle_suite_fut, cancellable_run_events_fut).await {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}

/// Runs tests specified in |test_params| and reports the results to
/// |run_reporter|.
///
/// Options specifying how the test run is executed are passed in via |run_params|.
/// Options specific to how a single suite is run are passed in via the entry for
/// the suite in |test_params|.
/// |cancel_fut| is used to gracefully stop execution of tests. Tests are
/// terminated and recorded when the future resolves. The caller can control when the
/// future resolves by passing in the receiver end of a `future::channel::oneshot`
/// channel.
pub async fn run_tests_and_get_outcome<F: Future<Output = ()>>(
    builder_proxy: RunBuilderProxy,
    test_params: impl IntoIterator<Item = TestParams>,
    run_params: RunParams,
    run_reporter: RunReporter,
    cancel_fut: F,
) -> Outcome {
    let test_outcome = match run_tests(
        builder_proxy,
        test_params,
        run_params,
        &run_reporter,
        cancel_fut.boxed_local(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            return Outcome::error(e);
        }
    };

    let report_result =
        match run_reporter.stopped(&test_outcome.clone().into(), Timestamp::Unknown).await {
            Ok(()) => run_reporter.finished().await,
            Err(e) => Err(e),
        };
    if let Err(e) = report_result {
        warn!("Failed to record results: {:?}", e);
    }

    test_outcome
}

pub struct DirectoryReporterOptions {
    /// Root path of the directory.
    pub root_path: PathBuf,
}

/// Create a reporter for use with |run_tests_and_get_outcome|.
pub fn create_reporter<W: 'static + Write + Send + Sync>(
    filter_ansi: bool,
    dir: Option<DirectoryReporterOptions>,
    writer: W,
) -> Result<output::RunReporter, anyhow::Error> {
    let stdout_reporter = output::ShellReporter::new(writer);
    let dir_reporter = dir
        .map(|dir| {
            output::DirectoryWithStdoutReporter::new(dir.root_path, output::SchemaVersion::V1)
        })
        .transpose()?;
    let reporter = match (dir_reporter, filter_ansi) {
        (Some(dir_reporter), false) => output::RunReporter::new(output::MultiplexedReporter::new(
            stdout_reporter,
            dir_reporter,
        )),
        (Some(dir_reporter), true) => output::RunReporter::new_ansi_filtered(
            output::MultiplexedReporter::new(stdout_reporter, dir_reporter),
        ),
        (None, false) => output::RunReporter::new(stdout_reporter),
        (None, true) => output::RunReporter::new_ansi_filtered(stdout_reporter),
    };
    Ok(reporter)
}

#[cfg(test)]
mod test {
    use {
        super::*, crate::output::InMemoryReporter, assert_matches::assert_matches,
        fidl::endpoints::create_proxy_and_stream, ftest_manager, futures::future::join,
        maplit::hashmap, output::EntityId,
    };
    #[cfg(target_os = "fuchsia")]
    use {
        fidl::endpoints::ServerEnd,
        fidl_fuchsia_io as fio, fuchsia_zircon as zx,
        futures::future::join3,
        vfs::{
            directory::entry::DirectoryEntry, execution_scope::ExecutionScope,
            file::vmo::read_only_static, pseudo_directory,
        },
    };

    async fn respond_to_get_events(
        request_stream: &mut ftest_manager::SuiteControllerRequestStream,
        events: Vec<ftest_manager::SuiteEvent>,
    ) {
        let request = request_stream
            .next()
            .await
            .expect("did not get next request")
            .expect("error getting next request");
        let responder = match request {
            ftest_manager::SuiteControllerRequest::GetEvents { responder } => responder,
            r => panic!("Expected GetEvents request but got {:?}", r),
        };

        responder.send(&mut Ok(events)).expect("send events");
    }

    /// Serves all events to completion.
    async fn serve_all_events(
        mut request_stream: ftest_manager::SuiteControllerRequestStream,
        events: Vec<ftest_manager::SuiteEvent>,
    ) {
        const BATCH_SIZE: usize = 5;
        let mut event_iter = events.into_iter();
        while event_iter.len() > 0 {
            respond_to_get_events(
                &mut request_stream,
                event_iter.by_ref().take(BATCH_SIZE).collect(),
            )
            .await;
        }
        respond_to_get_events(&mut request_stream, vec![]).await;
    }

    /// Serves all events to completion, then wait for the channel to close.
    async fn serve_all_events_then_hang(
        mut request_stream: ftest_manager::SuiteControllerRequestStream,
        events: Vec<ftest_manager::SuiteEvent>,
    ) {
        const BATCH_SIZE: usize = 5;
        let mut event_iter = events.into_iter();
        while event_iter.len() > 0 {
            respond_to_get_events(
                &mut request_stream,
                event_iter.by_ref().take(BATCH_SIZE).collect(),
            )
            .await;
        }
        let _requests = request_stream.collect::<Vec<_>>().await;
    }

    /// Creates a SuiteEvent which is unpopulated, except for timestamp.
    /// This isn't representative of an actual event from test framework, but is sufficient
    /// to assert events are routed correctly.
    fn create_empty_event(timestamp: i64) -> ftest_manager::SuiteEvent {
        ftest_manager::SuiteEvent { timestamp: Some(timestamp), ..ftest_manager::SuiteEvent::EMPTY }
    }

    macro_rules! assert_empty_events_eq {
        ($t1:expr, $t2:expr) => {
            assert_eq!($t1.timestamp, $t2.timestamp, "Got incorrect event.")
        };
    }

    #[fuchsia::test]
    async fn running_suite_events_simple() {
        let (suite_proxy, mut suite_request_stream) =
            create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>()
                .expect("create proxy");
        let suite_server_task = fasync::Task::spawn(async move {
            respond_to_get_events(&mut suite_request_stream, vec![create_empty_event(0)]).await;
            respond_to_get_events(&mut suite_request_stream, vec![]).await;
            drop(suite_request_stream);
        });

        let mut running_suite =
            RunningSuite::wait_for_start(suite_proxy, None, None, std::time::Duration::ZERO, None)
                .await;
        assert_empty_events_eq!(
            running_suite.event_stream.next().await.unwrap().unwrap(),
            create_empty_event(0)
        );
        assert!(running_suite.event_stream.next().await.is_none());
        // polling again should still give none.
        assert!(running_suite.event_stream.next().await.is_none());
        suite_server_task.await;
    }

    #[fuchsia::test]
    async fn running_suite_events_multiple_events() {
        let (suite_proxy, mut suite_request_stream) =
            create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>()
                .expect("create proxy");
        let suite_server_task = fasync::Task::spawn(async move {
            respond_to_get_events(
                &mut suite_request_stream,
                vec![create_empty_event(0), create_empty_event(1)],
            )
            .await;
            respond_to_get_events(
                &mut suite_request_stream,
                vec![create_empty_event(2), create_empty_event(3)],
            )
            .await;
            respond_to_get_events(&mut suite_request_stream, vec![]).await;
            drop(suite_request_stream);
        });

        let mut running_suite =
            RunningSuite::wait_for_start(suite_proxy, None, None, std::time::Duration::ZERO, None)
                .await;

        for num in 0..4 {
            assert_empty_events_eq!(
                running_suite.event_stream.next().await.unwrap().unwrap(),
                create_empty_event(num)
            );
        }
        assert!(running_suite.event_stream.next().await.is_none());
        suite_server_task.await;
    }

    #[fuchsia::test]
    async fn running_suite_events_peer_closed() {
        let (suite_proxy, mut suite_request_stream) =
            create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>()
                .expect("create proxy");
        let suite_server_task = fasync::Task::spawn(async move {
            respond_to_get_events(&mut suite_request_stream, vec![create_empty_event(1)]).await;
            drop(suite_request_stream);
        });

        let mut running_suite =
            RunningSuite::wait_for_start(suite_proxy, None, None, std::time::Duration::ZERO, None)
                .await;
        assert_empty_events_eq!(
            running_suite.event_stream.next().await.unwrap().unwrap(),
            create_empty_event(1)
        );
        assert_matches!(
            running_suite.event_stream.next().await,
            Some(Err(RunTestSuiteError::Fidl(fidl::Error::ClientChannelClosed { .. })))
        );
        suite_server_task.await;
    }

    fn suite_event_from_payload(
        timestamp: i64,
        payload: ftest_manager::SuiteEventPayload,
    ) -> ftest_manager::SuiteEvent {
        let mut event = create_empty_event(timestamp);
        event.payload = Some(payload);
        event
    }

    fn case_found_event(timestamp: i64, identifier: u32, name: &str) -> ftest_manager::SuiteEvent {
        suite_event_from_payload(
            timestamp,
            ftest_manager::SuiteEventPayload::CaseFound(ftest_manager::CaseFound {
                test_case_name: name.into(),
                identifier,
            }),
        )
    }

    fn case_started_event(timestamp: i64, identifier: u32) -> ftest_manager::SuiteEvent {
        suite_event_from_payload(
            timestamp,
            ftest_manager::SuiteEventPayload::CaseStarted(ftest_manager::CaseStarted {
                identifier,
            }),
        )
    }

    fn case_stopped_event(
        timestamp: i64,
        identifier: u32,
        status: ftest_manager::CaseStatus,
    ) -> ftest_manager::SuiteEvent {
        suite_event_from_payload(
            timestamp,
            ftest_manager::SuiteEventPayload::CaseStopped(ftest_manager::CaseStopped {
                identifier,
                status,
            }),
        )
    }

    fn case_finished_event(timestamp: i64, identifier: u32) -> ftest_manager::SuiteEvent {
        suite_event_from_payload(
            timestamp,
            ftest_manager::SuiteEventPayload::CaseFinished(ftest_manager::CaseFinished {
                identifier,
            }),
        )
    }

    fn case_stdout_event(
        timestamp: i64,
        identifier: u32,
        stdout: fidl::Socket,
    ) -> ftest_manager::SuiteEvent {
        suite_event_from_payload(
            timestamp,
            ftest_manager::SuiteEventPayload::CaseArtifact(ftest_manager::CaseArtifact {
                identifier,
                artifact: ftest_manager::Artifact::Stdout(stdout),
            }),
        )
    }

    fn case_stderr_event(
        timestamp: i64,
        identifier: u32,
        stderr: fidl::Socket,
    ) -> ftest_manager::SuiteEvent {
        suite_event_from_payload(
            timestamp,
            ftest_manager::SuiteEventPayload::CaseArtifact(ftest_manager::CaseArtifact {
                identifier,
                artifact: ftest_manager::Artifact::Stderr(stderr),
            }),
        )
    }

    fn suite_started_event(timestamp: i64) -> ftest_manager::SuiteEvent {
        suite_event_from_payload(
            timestamp,
            ftest_manager::SuiteEventPayload::SuiteStarted(ftest_manager::SuiteStarted),
        )
    }

    fn suite_stopped_event(
        timestamp: i64,
        status: ftest_manager::SuiteStatus,
    ) -> ftest_manager::SuiteEvent {
        suite_event_from_payload(
            timestamp,
            ftest_manager::SuiteEventPayload::SuiteStopped(ftest_manager::SuiteStopped { status }),
        )
    }

    #[fuchsia::test]
    async fn collect_suite_events_simple() {
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stopped_event(300, 0, ftest_manager::CaseStatus::Passed),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteStatus::Passed),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>()
            .expect("create stream");
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter =
                run_reporter.new_suite("test-url", &SuiteId(0)).await.expect("create new suite");

            let suite =
                RunningSuite::wait_for_start(proxy, None, None, std::time::Duration::ZERO, None)
                    .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                Outcome::Passed
            );
            suite_reporter.finished().await.expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { suite: SuiteId(0), case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(case.report.is_finished);
            assert!(case.report.artifacts.is_empty());
            assert!(case.report.directories.is_empty());
            let suite =
                reports.iter().find(|report| report.id == EntityId::Suite(SuiteId(0))).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join(serve_all_events(stream, all_events), test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_suite_events_with_case_artifacts() {
        const STDOUT_CONTENT: &str = "stdout from my_test_case";
        const STDERR_CONTENT: &str = "stderr from my_test_case";

        let (stdout_write, stdout_read) =
            fidl::Socket::create(fidl::SocketOpts::STREAM).expect("create socket");
        let (stderr_write, stderr_read) =
            fidl::Socket::create(fidl::SocketOpts::STREAM).expect("create socket");
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stdout_event(300, 0, stdout_read),
            case_stderr_event(300, 0, stderr_read),
            case_stopped_event(300, 0, ftest_manager::CaseStatus::Passed),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteStatus::Passed),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>()
            .expect("create stream");
        let stdio_write_fut = async move {
            let mut async_stdout =
                fasync::Socket::from_socket(stdout_write).expect("make async socket");
            async_stdout.write_all(STDOUT_CONTENT.as_bytes()).await.expect("write to socket");
            let mut async_stderr =
                fasync::Socket::from_socket(stderr_write).expect("make async socket");
            async_stderr.write_all(STDERR_CONTENT.as_bytes()).await.expect("write to socket");
        };
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter =
                run_reporter.new_suite("test-url", &SuiteId(0)).await.expect("create new suite");

            let suite =
                RunningSuite::wait_for_start(proxy, None, None, std::time::Duration::ZERO, None)
                    .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                Outcome::Passed
            );
            suite_reporter.finished().await.expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { suite: SuiteId(0), case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(case.report.is_finished);
            assert_eq!(case.report.artifacts.len(), 2);
            assert_eq!(
                case.report
                    .artifacts
                    .iter()
                    .map(|(artifact_type, artifact)| (*artifact_type, artifact.get_contents()))
                    .collect::<HashMap<_, _>>(),
                hashmap! {
                    output::ArtifactType::Stdout => STDOUT_CONTENT.as_bytes().to_vec(),
                    output::ArtifactType::Stderr => STDERR_CONTENT.as_bytes().to_vec()
                }
            );
            assert!(case.report.directories.is_empty());

            let suite =
                reports.iter().find(|report| report.id == EntityId::Suite(SuiteId(0))).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join3(serve_all_events(stream, all_events), stdio_write_fut, test_fut)
            .await;
    }

    #[fuchsia::test]
    async fn collect_suite_events_case_artifacts_complete_after_suite() {
        const STDOUT_CONTENT: &str = "stdout from my_test_case";
        const STDERR_CONTENT: &str = "stderr from my_test_case";

        let (stdout_write, stdout_read) =
            fidl::Socket::create(fidl::SocketOpts::STREAM).expect("create socket");
        let (stderr_write, stderr_read) =
            fidl::Socket::create(fidl::SocketOpts::STREAM).expect("create socket");
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stdout_event(300, 0, stdout_read),
            case_stderr_event(300, 0, stderr_read),
            case_stopped_event(300, 0, ftest_manager::CaseStatus::Passed),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteStatus::Passed),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>()
            .expect("create stream");
        let serve_fut = async move {
            // server side will send all events, then write to (and close) sockets.
            serve_all_events(stream, all_events).await;
            let mut async_stdout =
                fasync::Socket::from_socket(stdout_write).expect("make async socket");
            async_stdout.write_all(STDOUT_CONTENT.as_bytes()).await.expect("write to socket");
            let mut async_stderr =
                fasync::Socket::from_socket(stderr_write).expect("make async socket");
            async_stderr.write_all(STDERR_CONTENT.as_bytes()).await.expect("write to socket");
        };
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter =
                run_reporter.new_suite("test-url", &SuiteId(0)).await.expect("create new suite");

            let suite =
                RunningSuite::wait_for_start(proxy, None, None, std::time::Duration::ZERO, Some(1))
                    .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                Outcome::Passed
            );
            suite_reporter.finished().await.expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { suite: SuiteId(0), case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(case.report.is_finished);
            assert_eq!(case.report.artifacts.len(), 2);
            assert_eq!(
                case.report
                    .artifacts
                    .iter()
                    .map(|(artifact_type, artifact)| (*artifact_type, artifact.get_contents()))
                    .collect::<HashMap<_, _>>(),
                hashmap! {
                    output::ArtifactType::Stdout => STDOUT_CONTENT.as_bytes().to_vec(),
                    output::ArtifactType::Stderr => STDERR_CONTENT.as_bytes().to_vec()
                }
            );
            assert!(case.report.directories.is_empty());

            let suite =
                reports.iter().find(|report| report.id == EntityId::Suite(SuiteId(0))).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join(serve_fut, test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_suite_events_with_case_artifacts_sent_after_case_stopped() {
        const STDOUT_CONTENT: &str = "stdout from my_test_case";
        const STDERR_CONTENT: &str = "stderr from my_test_case";

        let (stdout_write, stdout_read) =
            fidl::Socket::create(fidl::SocketOpts::STREAM).expect("create socket");
        let (stderr_write, stderr_read) =
            fidl::Socket::create(fidl::SocketOpts::STREAM).expect("create socket");
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stopped_event(300, 0, ftest_manager::CaseStatus::Passed),
            case_stdout_event(300, 0, stdout_read),
            case_stderr_event(300, 0, stderr_read),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteStatus::Passed),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>()
            .expect("create stream");
        let stdio_write_fut = async move {
            let mut async_stdout =
                fasync::Socket::from_socket(stdout_write).expect("make async socket");
            async_stdout.write_all(STDOUT_CONTENT.as_bytes()).await.expect("write to socket");
            let mut async_stderr =
                fasync::Socket::from_socket(stderr_write).expect("make async socket");
            async_stderr.write_all(STDERR_CONTENT.as_bytes()).await.expect("write to socket");
        };
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter =
                run_reporter.new_suite("test-url", &SuiteId(0)).await.expect("create new suite");

            let suite =
                RunningSuite::wait_for_start(proxy, None, None, std::time::Duration::ZERO, None)
                    .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                Outcome::Passed
            );
            suite_reporter.finished().await.expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { suite: SuiteId(0), case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(case.report.is_finished);
            assert_eq!(case.report.artifacts.len(), 2);
            assert_eq!(
                case.report
                    .artifacts
                    .iter()
                    .map(|(artifact_type, artifact)| (*artifact_type, artifact.get_contents()))
                    .collect::<HashMap<_, _>>(),
                hashmap! {
                    output::ArtifactType::Stdout => STDOUT_CONTENT.as_bytes().to_vec(),
                    output::ArtifactType::Stderr => STDERR_CONTENT.as_bytes().to_vec()
                }
            );
            assert!(case.report.directories.is_empty());

            let suite =
                reports.iter().find(|report| report.id == EntityId::Suite(SuiteId(0))).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join3(serve_all_events(stream, all_events), stdio_write_fut, test_fut)
            .await;
    }

    #[fuchsia::test]
    async fn collect_suite_events_timed_out_case_with_hanging_artifacts() {
        // create sockets and leave the server end open to simulate a hang.
        let (_stdout_write, stdout_read) =
            fidl::Socket::create(fidl::SocketOpts::STREAM).expect("create socket");
        let (_stderr_write, stderr_read) =
            fidl::Socket::create(fidl::SocketOpts::STREAM).expect("create socket");
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stdout_event(300, 0, stdout_read),
            case_stderr_event(300, 0, stderr_read),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>()
            .expect("create stream");
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter =
                run_reporter.new_suite("test-url", &SuiteId(0)).await.expect("create new suite");

            let suite = RunningSuite::wait_for_start(
                proxy,
                None,
                Some(std::time::Duration::from_secs(2)),
                std::time::Duration::ZERO,
                None,
            )
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                Outcome::Timedout
            );
            suite_reporter.finished().await.expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { suite: SuiteId(0), case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Timedout));
            assert!(case.report.is_finished);
            assert_eq!(case.report.artifacts.len(), 2);
            assert_eq!(
                case.report
                    .artifacts
                    .iter()
                    .map(|(artifact_type, artifact)| (*artifact_type, artifact.get_contents()))
                    .collect::<HashMap<_, _>>(),
                hashmap! {
                    output::ArtifactType::Stdout => vec![],
                    output::ArtifactType::Stderr => vec![]
                }
            );
            assert!(case.report.directories.is_empty());

            let suite =
                reports.iter().find(|report| report.id == EntityId::Suite(SuiteId(0))).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Timedout));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join(serve_all_events_then_hang(stream, all_events), test_fut).await;
    }

    // TODO(fxbug.dev/98222): add unit tests for suite artifacts too.

    async fn fake_running_all_suites_and_return_run_events(
        mut stream: ftest_manager::RunBuilderRequestStream,
        mut suite_events: HashMap<&str, Vec<ftest_manager::SuiteEvent>>,
        run_events: Vec<ftest_manager::RunEvent>,
    ) {
        let mut suite_streams = vec![];

        let mut run_controller = None;
        while let Ok(Some(req)) = stream.try_next().await {
            match req {
                ftest_manager::RunBuilderRequest::AddSuite { test_url, controller, .. } => {
                    let events = suite_events
                        .remove(test_url.as_str())
                        .expect("Got a request for an unexpected test URL");
                    suite_streams.push((controller.into_stream().expect("into stream"), events));
                }
                ftest_manager::RunBuilderRequest::Build { controller, .. } => {
                    run_controller = Some(controller);
                    break;
                }
                ftest_manager::RunBuilderRequest::WithSchedulingOptions { options, .. } => {
                    if let Some(_) = options.max_parallel_suites {
                        panic!("Not expecting calls to WithSchedulingOptions where options.max_parallel_suites is Some()")
                    }
                }
            }
        }
        assert!(
            run_controller.is_some(),
            "Expected a RunController to be present. RunBuilder/Build() may not have been called."
        );
        assert!(suite_events.is_empty(), "Expected AddSuite to be called for all specified suites");
        let mut run_stream =
            run_controller.expect("controller present").into_stream().expect("into stream");

        // Each suite just reports that it started and passed.
        let mut suite_streams = suite_streams
            .into_iter()
            .map(|(mut stream, events)| {
                async move {
                    let mut maybe_events = Some(events);
                    while let Ok(Some(req)) = stream.try_next().await {
                        match req {
                            ftest_manager::SuiteControllerRequest::GetEvents {
                                responder, ..
                            } => {
                                let send_events = maybe_events.take().unwrap_or(vec![]);
                                let _ = responder.send(&mut Ok(send_events));
                            }
                            _ => {
                                // ignore all other requests
                            }
                        }
                    }
                }
            })
            .collect::<FuturesUnordered<_>>();

        let suite_drain_fut = async move { while let Some(_) = suite_streams.next().await {} };

        let run_fut = async move {
            let mut events = Some(run_events);
            while let Ok(Some(req)) = run_stream.try_next().await {
                match req {
                    ftest_manager::RunControllerRequest::GetEvents { responder, .. } => {
                        if events.is_none() {
                            let _ = responder.send(&mut vec![].into_iter());
                            continue;
                        }
                        let events = events.take().unwrap();
                        let _ = responder.send(&mut events.into_iter());
                    }
                    _ => {
                        // ignore all other requests
                    }
                }
            }
        };

        join(suite_drain_fut, run_fut).await;
    }

    struct ParamsForRunTests {
        builder_proxy: ftest_manager::RunBuilderProxy,
        test_params: Vec<TestParams>,
        run_reporter: RunReporter,
    }

    fn create_empty_suite_events() -> Vec<ftest_manager::SuiteEvent> {
        vec![
            ftest_manager::SuiteEvent {
                timestamp: Some(1000),
                payload: Some(ftest_manager::SuiteEventPayload::SuiteStarted(
                    ftest_manager::SuiteStarted,
                )),
                ..ftest_manager::SuiteEvent::EMPTY
            },
            ftest_manager::SuiteEvent {
                timestamp: Some(2000),
                payload: Some(ftest_manager::SuiteEventPayload::SuiteStopped(
                    ftest_manager::SuiteStopped { status: ftest_manager::SuiteStatus::Passed },
                )),
                ..ftest_manager::SuiteEvent::EMPTY
            },
        ]
    }

    async fn call_run_tests(params: ParamsForRunTests) -> Outcome {
        run_tests_and_get_outcome(
            params.builder_proxy,
            params.test_params,
            RunParams {
                timeout_behavior: TimeoutBehavior::Continue,
                timeout_grace_seconds: 0,
                stop_after_failures: None,
                experimental_parallel_execution: None,
                accumulate_debug_data: false,
                log_protocol: None,
                min_severity_logs: None,
                show_full_moniker: false,
            },
            params.run_reporter,
            futures::future::pending(),
        )
        .await
    }

    #[fuchsia::test]
    async fn empty_run_no_events() {
        let (builder_proxy, run_builder_stream) =
            create_proxy_and_stream::<ftest_manager::RunBuilderMarker>()
                .expect("create builder proxy");

        let reporter = InMemoryReporter::new();
        let run_reporter = RunReporter::new(reporter.clone());
        let run_fut =
            call_run_tests(ParamsForRunTests { builder_proxy, test_params: vec![], run_reporter });
        let fake_fut =
            fake_running_all_suites_and_return_run_events(run_builder_stream, hashmap! {}, vec![]);

        assert_eq!(join(run_fut, fake_fut).await.0, Outcome::Passed,);

        let reports = reporter.get_reports();
        assert_eq!(1usize, reports.len());
        assert_eq!(reports[0].id, EntityId::TestRun);
    }

    #[fuchsia::test]
    async fn single_run_no_events() {
        let (builder_proxy, run_builder_stream) =
            create_proxy_and_stream::<ftest_manager::RunBuilderMarker>()
                .expect("create builder proxy");

        let reporter = InMemoryReporter::new();
        let run_reporter = RunReporter::new(reporter.clone());
        let run_fut = call_run_tests(ParamsForRunTests {
            builder_proxy,
            test_params: vec![TestParams {
                test_url: "fuchsia-pkg://fuchsia.com/nothing#meta/nothing.cm".to_string(),
                ..TestParams::default()
            }],
            run_reporter,
        });
        let fake_fut = fake_running_all_suites_and_return_run_events(
            run_builder_stream,
            hashmap! {
                "fuchsia-pkg://fuchsia.com/nothing#meta/nothing.cm" => create_empty_suite_events()
            },
            vec![],
        );

        assert_eq!(join(run_fut, fake_fut).await.0, Outcome::Passed,);

        let reports = reporter.get_reports();
        assert_eq!(2usize, reports.len());
        assert!(reports[0].report.artifacts.is_empty());
        assert!(reports[0].report.directories.is_empty());
        assert!(reports[1].report.artifacts.is_empty());
        assert!(reports[1].report.directories.is_empty());
    }

    #[cfg(target_os = "fuchsia")]
    #[fuchsia::test]
    async fn single_run_custom_directory() {
        let (builder_proxy, run_builder_stream) =
            create_proxy_and_stream::<ftest_manager::RunBuilderMarker>()
                .expect("create builder proxy");

        let reporter = InMemoryReporter::new();
        let run_reporter = RunReporter::new(reporter.clone());
        let run_fut = call_run_tests(ParamsForRunTests {
            builder_proxy,
            test_params: vec![TestParams {
                test_url: "fuchsia-pkg://fuchsia.com/nothing#meta/nothing.cm".to_string(),
                ..TestParams::default()
            }],
            run_reporter,
        });

        let dir = pseudo_directory! {
            "test_file.txt" => read_only_static("Hello, World!"),
        };

        let (directory_client, directory_service) =
            fidl::endpoints::create_endpoints::<fio::DirectoryMarker>().unwrap();
        let scope = ExecutionScope::new();
        dir.open(
            scope,
            fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
            fio::MODE_TYPE_DIRECTORY,
            vfs::path::Path::dot(),
            ServerEnd::new(directory_service.into_channel()),
        );

        let (_pair_1, pair_2) = zx::EventPair::create().unwrap();

        let events = vec![ftest_manager::RunEvent {
            payload: Some(ftest_manager::RunEventPayload::Artifact(
                ftest_manager::Artifact::Custom(ftest_manager::CustomArtifact {
                    directory_and_token: Some(ftest_manager::DirectoryAndToken {
                        directory: directory_client,
                        token: pair_2,
                    }),
                    ..ftest_manager::CustomArtifact::EMPTY
                }),
            )),
            ..ftest_manager::RunEvent::EMPTY
        }];

        let fake_fut = fake_running_all_suites_and_return_run_events(
            run_builder_stream,
            hashmap! {
                "fuchsia-pkg://fuchsia.com/nothing#meta/nothing.cm" => create_empty_suite_events()
            },
            events,
        );

        assert_eq!(join(run_fut, fake_fut).await.0, Outcome::Passed,);

        let reports = reporter.get_reports();
        assert_eq!(2usize, reports.len());
        let run = reports.iter().find(|e| e.id == EntityId::TestRun).expect("find run report");
        assert_eq!(1usize, run.report.directories.len());
        let dir = &run.report.directories[0];
        let files = dir.1.files.lock();
        assert_eq!(1usize, files.len());
        let (name, file) = &files[0];
        assert_eq!(name.to_string_lossy(), "test_file.txt".to_string());
        assert_eq!(file.get_contents(), b"Hello, World!");
    }

    #[fuchsia::test]
    async fn record_output_after_internal_error() {
        let (builder_proxy, run_builder_stream) =
            create_proxy_and_stream::<ftest_manager::RunBuilderMarker>()
                .expect("create builder proxy");

        let reporter = InMemoryReporter::new();
        let run_reporter = RunReporter::new(reporter.clone());
        let run_fut = call_run_tests(ParamsForRunTests {
            builder_proxy,
            test_params: vec![
                TestParams {
                    test_url: "fuchsia-pkg://fuchsia.com/invalid#meta/invalid.cm".to_string(),
                    ..TestParams::default()
                },
                TestParams {
                    test_url: "fuchsia-pkg://fuchsia.com/nothing#meta/nothing.cm".to_string(),
                    ..TestParams::default()
                },
            ],
            run_reporter,
        });

        let fake_fut = fake_running_all_suites_and_return_run_events(
            run_builder_stream,
            hashmap! {
                // return an internal error from the first test.
                "fuchsia-pkg://fuchsia.com/invalid#meta/invalid.cm" => vec![
                    ftest_manager::SuiteEvent {
                        timestamp: Some(1000),
                        payload: Some(
                            ftest_manager::SuiteEventPayload::SuiteStarted(
                                ftest_manager::SuiteStarted,
                            ),
                        ),
                        ..ftest_manager::SuiteEvent::EMPTY
                    },
                    ftest_manager::SuiteEvent {
                        timestamp: Some(2000),
                        payload: Some(ftest_manager::SuiteEventPayload::SuiteStopped(
                            ftest_manager::SuiteStopped { status: ftest_manager::SuiteStatus::InternalError },
                        )),
                        ..ftest_manager::SuiteEvent::EMPTY
                    },
                ],
                "fuchsia-pkg://fuchsia.com/nothing#meta/nothing.cm" => create_empty_suite_events()
            },
            vec![],
        );

        assert_matches!(join(run_fut, fake_fut).await.0, Outcome::Error { .. });

        let reports = reporter.get_reports();
        assert_eq!(3usize, reports.len());
        let invalid_suite = reports
            .iter()
            .find(|e| e.report.name == "fuchsia-pkg://fuchsia.com/invalid#meta/invalid.cm")
            .expect("find run report");
        assert_eq!(invalid_suite.report.outcome, Some(output::ReportedOutcome::Error));
        assert!(invalid_suite.report.is_finished);

        // The valid suite should not have been started, but finish should've been called so that
        // results get persisted.
        let not_started = reports
            .iter()
            .find(|e| e.report.name == "fuchsia-pkg://fuchsia.com/nothing#meta/nothing.cm")
            .expect("find run report");
        assert!(not_started.report.outcome.is_none());
        assert!(not_started.report.is_finished);

        // The results for the run should also be saved.
        let run = reports.iter().find(|e| e.id == EntityId::TestRun).expect("find run report");
        assert_eq!(run.report.outcome, Some(output::ReportedOutcome::Error));
        assert!(run.report.is_finished);
        assert!(run.report.started_time.is_some());
    }

    #[cfg(target_os = "fuchsia")]
    #[fuchsia::test]
    async fn single_run_debug_data() {
        let (builder_proxy, run_builder_stream) =
            create_proxy_and_stream::<ftest_manager::RunBuilderMarker>()
                .expect("create builder proxy");

        let reporter = InMemoryReporter::new();
        let run_reporter = RunReporter::new(reporter.clone());
        let run_fut = call_run_tests(ParamsForRunTests {
            builder_proxy,
            test_params: vec![TestParams {
                test_url: "fuchsia-pkg://fuchsia.com/nothing#meta/nothing.cm".to_string(),
                ..TestParams::default()
            }],
            run_reporter,
        });

        let dir = pseudo_directory! {
            "test_file.profraw" => read_only_static("Not a real profile"),
        };

        let (file_client, file_service) =
            fidl::endpoints::create_endpoints::<fio::FileMarker>().unwrap();
        let scope = ExecutionScope::new();
        dir.open(
            scope,
            fio::OpenFlags::RIGHT_READABLE,
            fio::MODE_TYPE_FILE,
            vfs::path::Path::validate_and_split("test_file.profraw").unwrap(),
            ServerEnd::new(file_service.into_channel()),
        );

        let (debug_client, debug_service) =
            fidl::endpoints::create_endpoints::<ftest_manager::DebugDataIteratorMarker>().unwrap();
        let debug_data_fut = async move {
            let mut service = debug_service.into_stream().unwrap();
            let mut data = vec![ftest_manager::DebugData {
                name: Some("test_file.profraw".to_string()),
                file: Some(file_client),
                ..ftest_manager::DebugData::EMPTY
            }];
            while let Ok(Some(request)) = service.try_next().await {
                match request {
                    ftest_manager::DebugDataIteratorRequest::GetNext { responder, .. } => {
                        let _ = responder.send(&mut data.drain(0..));
                    }
                }
            }
        };
        let events = vec![ftest_manager::RunEvent {
            payload: Some(ftest_manager::RunEventPayload::Artifact(
                ftest_manager::Artifact::DebugData(debug_client),
            )),
            ..ftest_manager::RunEvent::EMPTY
        }];

        let fake_fut = fake_running_all_suites_and_return_run_events(
            run_builder_stream,
            hashmap! {

                "fuchsia-pkg://fuchsia.com/nothing#meta/nothing.cm" => create_empty_suite_events(),
            },
            events,
        );

        assert_eq!(join3(run_fut, debug_data_fut, fake_fut).await.0, Outcome::Passed);

        let reports = reporter.get_reports();
        assert_eq!(2usize, reports.len());
        let run = reports.iter().find(|e| e.id == EntityId::TestRun).expect("find run report");
        assert_eq!(1usize, run.report.directories.len());
        let dir = &run.report.directories[0];
        let files = dir.1.files.lock();
        assert_eq!(1usize, files.len());
        let (name, file) = &files[0];
        assert_eq!(name.to_string_lossy(), "test_file.profraw".to_string());
        assert_eq!(file.get_contents(), b"Not a real profile");
    }

    async fn fake_parallel_options_server(
        mut stream: ftest_manager::RunBuilderRequestStream,
    ) -> Option<ftest_manager::SchedulingOptions> {
        let mut scheduling_options = None;
        if let Ok(Some(req)) = stream.try_next().await {
            match req {
                ftest_manager::RunBuilderRequest::AddSuite { .. } => {
                    panic!("Not expecting an AddSuite request")
                }
                ftest_manager::RunBuilderRequest::Build { .. } => {
                    panic!("Not expecting a Build request")
                }
                ftest_manager::RunBuilderRequest::WithSchedulingOptions { options, .. } => {
                    scheduling_options = Some(options);
                }
            }
        }
        scheduling_options
    }

    #[fuchsia::test]
    async fn request_scheduling_options_test_parallel() {
        let max_parallel_suites: u16 = 10;
        let expected_max_parallel_suites = Some(max_parallel_suites);

        let (builder_proxy, run_builder_stream) =
            create_proxy_and_stream::<ftest_manager::RunBuilderMarker>()
                .expect("create builder proxy");

        let run_params = RunParams {
            timeout_behavior: TimeoutBehavior::Continue,
            timeout_grace_seconds: 0,
            stop_after_failures: None,
            experimental_parallel_execution: Some(max_parallel_suites),
            accumulate_debug_data: false,
            log_protocol: None,
            min_severity_logs: None,
            show_full_moniker: false,
        };

        let request_parallel_fut = request_scheduling_options(&run_params, &builder_proxy);
        let fake_server_fut = fake_parallel_options_server(run_builder_stream);

        let returned_options = join(request_parallel_fut, fake_server_fut).await.1;
        let max_parallel_suites_received = match returned_options {
            Some(scheduling_options) => scheduling_options.max_parallel_suites,
            None => panic!("Expected scheduling options."),
        };
        assert_eq!(max_parallel_suites_received, expected_max_parallel_suites);
    }

    #[fuchsia::test]
    async fn request_scheduling_options_test_serial() {
        let expected_max_parallel_suites = None;

        let (builder_proxy, run_builder_stream) =
            create_proxy_and_stream::<ftest_manager::RunBuilderMarker>()
                .expect("create builder proxy");

        let run_params = RunParams {
            timeout_behavior: TimeoutBehavior::Continue,
            timeout_grace_seconds: 0,
            stop_after_failures: None,
            experimental_parallel_execution: None,
            accumulate_debug_data: false,
            log_protocol: None,
            min_severity_logs: None,
            show_full_moniker: false,
        };

        let request_parallel_fut = request_scheduling_options(&run_params, &builder_proxy);
        let fake_server_fut = fake_parallel_options_server(run_builder_stream);

        let returned_options = join(request_parallel_fut, fake_server_fut)
            .await
            .1
            .expect("Expected scheduling options.");
        let max_parallel_suites_received = returned_options.max_parallel_suites;
        assert_eq!(max_parallel_suites_received, expected_max_parallel_suites);
    }
}
