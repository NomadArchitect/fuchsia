// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fidl_fuchsia_hardware_power_statecontrol::{
    AdminPerformRebootResult, AdminProxy, AdminRequest, AdminRequestStream, RebootOptions,
};
use fuchsia_async as fasync;
use futures::{TryFutureExt, TryStreamExt};
use std::sync::Arc;

pub struct MockRebootService {
    call_hook: Box<dyn Fn(RebootOptions) -> AdminPerformRebootResult + Send + Sync>,
}

impl MockRebootService {
    /// Creates a new MockRebootService with a given callback to run per call to the service.
    /// `call_hook` must return a `Result` for each call, which will be sent to
    /// the caller as the result of the reboot call.
    pub fn new(
        call_hook: Box<dyn Fn(RebootOptions) -> AdminPerformRebootResult + Send + Sync>,
    ) -> Self {
        Self { call_hook }
    }

    /// Serves only the reboot portion of the fuchsia.hardware.power.statecontrol protocol on the
    /// given request stream.
    pub async fn run_reboot_service(
        self: Arc<Self>,
        mut stream: AdminRequestStream,
    ) -> Result<(), Error> {
        while let Some(event) = stream.try_next().await.expect("received request") {
            match event {
                AdminRequest::PerformReboot { options, responder } => {
                    let result = (self.call_hook)(options);
                    responder.send(result)?;
                }
                _ => {
                    panic!("unhandled RebootService method {event:?}");
                }
            }
        }
        Ok(())
    }

    /// Spawns and detaches a Fuchsia async Task which serves the reboot portion of the
    /// fuchsia.hardware.power.statecontrol protocol, returning a proxy directly.
    pub fn spawn_reboot_service(self: Arc<Self>) -> AdminProxy {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<
            fidl_fuchsia_hardware_power_statecontrol::AdminMarker,
        >();

        fasync::Task::spawn(
            self.run_reboot_service(stream)
                .unwrap_or_else(|e| panic!("error running reboot service: {e:?}")),
        )
        .detach();

        proxy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_hardware_power_statecontrol::RebootReason2;
    use fuchsia_async as fasync;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[fasync::run_singlethreaded(test)]
    async fn test_mock_reboot() {
        let reboot_service = Arc::new(MockRebootService::new(Box::new(|_| Ok(()))));

        let reboot_service_clone = Arc::clone(&reboot_service);
        let proxy = reboot_service_clone.spawn_reboot_service();

        proxy
            .perform_reboot(&RebootOptions {
                reasons: Some(vec![RebootReason2::SystemUpdate]),
                ..Default::default()
            })
            .await
            .expect("made reboot call")
            .expect("reboot call succeeded");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_mock_reboot_fails() {
        let reboot_service =
            Arc::new(MockRebootService::new(Box::new(|_| Err(zx::Status::INTERNAL.into_raw()))));

        let reboot_service_clone = Arc::clone(&reboot_service);
        let proxy = reboot_service_clone.spawn_reboot_service();

        let reboot_result = proxy
            .perform_reboot(&RebootOptions {
                reasons: Some(vec![RebootReason2::SystemUpdate]),
                ..Default::default()
            })
            .await
            .expect("made reboot call");
        assert_eq!(reboot_result, Err(zx::Status::INTERNAL.into_raw()));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_mock_reboot_call_hook() {
        let reboot_service = Arc::new(MockRebootService::new(Box::new(|options| {
            if let Some(reasons) = options.reasons {
                match &reasons[..] {
                    [RebootReason2::UserRequest] => Ok(()),
                    _ => Err(zx::Status::NOT_SUPPORTED.into_raw()),
                }
            } else {
                Err(zx::Status::NOT_SUPPORTED.into_raw())
            }
        })));

        let reboot_service_clone = Arc::clone(&reboot_service);
        let proxy = reboot_service_clone.spawn_reboot_service();

        // Succeed when given expected reboot reason.
        let () = proxy
            .perform_reboot(&RebootOptions {
                reasons: Some(vec![RebootReason2::UserRequest]),
                ..Default::default()
            })
            .await
            .expect("made reboot call")
            .expect("reboot call succeeded");

        // Error when given unexpected reboot reason.
        let error_reboot_result = proxy
            .perform_reboot(&RebootOptions {
                reasons: Some(vec![RebootReason2::SystemUpdate]),
                ..Default::default()
            })
            .await
            .expect("made reboot call");
        assert_eq!(error_reboot_result, Err(zx::Status::NOT_SUPPORTED.into_raw()));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_mock_reboot_with_external_state() {
        let called = Arc::new(AtomicU32::new(0));
        let called_clone = Arc::clone(&called);
        let reboot_service = Arc::new(MockRebootService::new(Box::new(move |_| {
            called_clone.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));

        let reboot_service_clone = Arc::clone(&reboot_service);
        let proxy = reboot_service_clone.spawn_reboot_service();

        proxy
            .perform_reboot(&RebootOptions {
                reasons: Some(vec![RebootReason2::SystemUpdate]),
                ..Default::default()
            })
            .await
            .expect("made reboot call")
            .expect("reboot call succeeded");
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }
}
