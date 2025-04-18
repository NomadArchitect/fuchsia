// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_utils::hanging_get::client::HangingGetStream;
use core::pin::Pin;
use core::task::{Context, Poll};
use fidl_fuchsia_bluetooth_sys as sys;
use fuchsia_bluetooth::types::{Address, HostInfo};
use fuchsia_bluetooth::Error;
use futures::ready;
use futures::stream::{FusedStream, Stream, StreamExt};
use log::trace;

/// Item type returned by `<HostWatcher as Stream>::poll_next`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HostEvent {
    /// The existing active host changed discoverable state.
    Discoverable(bool),
    /// There is a new active host.
    NewActiveHost { discoverable: bool },
    /// There is no active host.
    NotAvailable,
}

/// The termination status of the stream.
#[derive(Debug, Eq, PartialEq, PartialOrd, Ord)]
enum Terminated {
    Active,
    ShuttingDown,
    Done,
}

/// Watches for changes in state in the active Bluetooth Host of the system.
/// `HostWatcher` implements Stream. The `HostWatcher` _must_ be polled to receive updates about
/// the currently active host.
pub struct HostWatcher {
    /// Hanging-get client wrapper to watch for changes in Host state.
    host_updates: HangingGetStream<sys::HostWatcherProxy, Vec<sys::HostInfo>>,
    /// Information about the currently active Host, or None if there is no such Host.
    active_host: Option<HostInfo>,
    /// Termination status of the `host_updates` watcher.
    terminated: Terminated,
}

impl HostWatcher {
    /// Returns a HostWatcher that watches for changes in state of the currently active Bluetooth
    /// Host.
    pub fn new(host_watcher: sys::HostWatcherProxy) -> Self {
        let host_updates =
            HangingGetStream::new_with_fn_ptr(host_watcher, sys::HostWatcherProxy::watch);
        Self { host_updates, active_host: None, terminated: Terminated::Active }
    }

    #[cfg(test)]
    fn new_with_state(host_watcher: sys::HostWatcherProxy, active_host: Option<HostInfo>) -> Self {
        let mut this = Self::new(host_watcher);
        this.active_host = active_host;
        this
    }

    pub fn set_active_host(&mut self, host: HostInfo) {
        self.active_host = Some(host);
    }

    // Compares the `new` host state to the current and returns a HostEvent if the relevant state
    // has changed.
    fn compare(&self, new: &Option<HostInfo>) -> Option<HostEvent> {
        trace!("Current ({:?}) - New ({:?})", self.active_host, new);
        match (&self.active_host, new) {
            (None, Some(info)) => {
                Some(HostEvent::NewActiveHost { discoverable: info.discoverable })
            }
            (Some(_), None) => Some(HostEvent::NotAvailable),
            (Some(current_info), Some(new_info)) if current_info.id != new_info.id => {
                Some(HostEvent::NewActiveHost { discoverable: new_info.discoverable })
            }
            (Some(current_info), Some(new_info))
                if current_info.discoverable != new_info.discoverable =>
            {
                // The host discoverable state changed.
                Some(HostEvent::Discoverable(new_info.discoverable))
            }
            _ => None, // Otherwise, there was no change in host availability or state.
        }
    }

    fn handle_host_watcher_update(
        &mut self,
        update: Vec<sys::HostInfo>,
    ) -> Result<Option<HostEvent>, Error> {
        let maybe_active = update
            .iter()
            .find(|info| info.active.unwrap_or(false))
            .map(HostInfo::try_from)
            .transpose()?;

        let event = self.compare(&maybe_active);
        self.active_host = maybe_active;
        return Ok(event);
    }

    /// Returns all the known addresses of the active Host, or None if not set.
    pub fn addresses(&self) -> Option<Vec<Address>> {
        self.active_host.as_ref().map(|host| host.addresses.clone())
    }

    /// Returns the public address of the active Host, or None if not set.
    pub fn public_address(&self) -> Option<Address> {
        self.active_host
            .as_ref()
            .map(|host| {
                host.addresses.iter().find(|addr| matches!(addr, Address::Public(_))).copied()
            })
            .flatten()
    }

    /// Returns the BLE address of the active Host, or None if not set.
    pub fn ble_address(&self) -> Option<Address> {
        self.addresses()
            .map(|addrs| addrs.into_iter().find(|addr| matches!(addr, Address::Random(_))))
            .flatten()
            .or_else(|| self.public_address())
    }

    /// Returns the current discoverable state of the active Host, or None if not set.
    pub fn pairing_mode(&self) -> Option<bool> {
        self.active_host.as_ref().map(|h| h.discoverable)
    }

    pub fn local_name(&self) -> Option<String> {
        self.active_host.as_ref().map(|h| h.local_name.clone()).flatten()
    }
}

impl Stream for HostWatcher {
    type Item = Result<HostEvent, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.terminated {
            Terminated::Active => {}
            Terminated::ShuttingDown => {
                self.terminated = Terminated::Done;
                return Poll::Ready(None);
            }
            Terminated::Done => panic!("Cannot poll a terminated stream"),
        }

        // Keep polling the request stream until it produces a request that should be returned or it
        // produces Poll::Pending.
        loop {
            let result = ready!(self.host_updates.poll_next_unpin(cx));

            let result = match result {
                Some(Ok(update)) => match self.handle_host_watcher_update(update) {
                    Ok(None) => continue,
                    Ok(Some(request)) => Some(Ok(request)),
                    Err(e) => Some(Err(e)),
                },
                Some(Err(e)) => {
                    // FIDL errors are typically irrecoverable - return the Error and stage stream
                    // for termination. The next time it is polled, it will complete.
                    self.terminated = Terminated::ShuttingDown;
                    Some(Err(e.into()))
                }
                None => None,
            };
            if result.is_none() {
                trace!("HostWatcher hanging-get exhausted");
                self.terminated = Terminated::Done;
            }

            return Poll::Ready(result);
        }
    }
}

impl FusedStream for HostWatcher {
    fn is_terminated(&self) -> bool {
        self.terminated == Terminated::Done
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use assert_matches::assert_matches;
    use async_utils::PollExt;
    use fuchsia_async as fasync;
    use fuchsia_bluetooth::types::{example_host, HostId};
    use std::pin::pin;

    #[track_caller]
    fn expect_watch_request(
        exec: &mut fasync::TestExecutor,
        stream: &mut sys::HostWatcherRequestStream,
    ) -> sys::HostWatcherWatchResponder {
        let expect_fut = stream.select_next_some();
        let mut expect_fut = pin!(expect_fut);
        exec.run_until_stalled(&mut expect_fut)
            .expect("ready")
            .expect("valid FIDL request")
            .into_watch()
            .expect("Watch request")
    }

    #[fuchsia::test]
    fn update_with_no_hosts_stream_is_pending() {
        let mut exec = fasync::TestExecutor::new();

        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<sys::HostWatcherMarker>();
        let mut watcher = HostWatcher::new(proxy);

        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");

        let watch_responder = expect_watch_request(&mut exec, &mut server);
        // Respond with no hosts.
        let _ = watch_responder.send(&[]).unwrap();

        // By default, there are no hosts, so when the upstream watcher responds with no hosts, the
        // watcher stream should not yield an event.
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");
    }

    #[fuchsia::test]
    fn update_with_active_host_change_yields_items() {
        let mut exec = fasync::TestExecutor::new();

        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<sys::HostWatcherMarker>();
        let mut watcher = HostWatcher::new(proxy);

        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");

        let watch_responder = expect_watch_request(&mut exec, &mut server);
        // Respond with an active host.
        let host1 =
            example_host(HostId(1), /* active= */ true, /* discoverable= */ false);
        let _ = watch_responder.send(&[host1]).unwrap();

        // HostWatcher stream should yield a change in host state.
        let item = exec.run_until_stalled(&mut watcher.next()).expect("host update ready");
        assert_matches!(item, Some(Ok(HostEvent::NewActiveHost { discoverable: false })));

        // Because this is a hanging-get, we expect the HostWatcher to make the next request.
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");
        let watch_responder = expect_watch_request(&mut exec, &mut server);
        // Respond with no hosts.
        let _ = watch_responder.send(&[]).unwrap();

        // HostWatcher stream should yield a change in host state.
        let item = exec.run_until_stalled(&mut watcher.next()).expect("host update");
        assert_matches!(item, Some(Ok(HostEvent::NotAvailable)));
    }

    #[fuchsia::test]
    fn active_to_no_active_host_update_yields_event() {
        let mut exec = fasync::TestExecutor::new();

        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<sys::HostWatcherMarker>();
        let host = example_host(HostId(1), /* active= */ true, /* discoverable= */ false);
        // HostWatcher starts off with a tracked active, non-discoverable host.
        let mut watcher = HostWatcher::new_with_state(proxy, host.try_into().ok());
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");

        let watch_responder = expect_watch_request(&mut exec, &mut server);
        // Respond with no active hosts.
        let host2 =
            example_host(HostId(2), /* active= */ false, /* discoverable= */ false);
        let host3 =
            example_host(HostId(3), /* active= */ false, /* discoverable= */ false);
        let _ = watch_responder.send(&[host2, host3]).unwrap();

        // HostWatcher stream should yield a change in host state since it went from active host
        // to no active host.
        let item = exec.run_until_stalled(&mut watcher.next()).expect("host update");
        assert_matches!(item, Some(Ok(HostEvent::NotAvailable)));
    }

    #[fuchsia::test]
    fn update_with_no_active_host_changes_is_pending() {
        let mut exec = fasync::TestExecutor::new();

        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<sys::HostWatcherMarker>();
        // HostWatcher starts off with a tracked active, non-discoverable host.
        let mut host1 =
            example_host(HostId(1), /* active= */ true, /* discoverable= */ false);
        let mut watcher = HostWatcher::new_with_state(proxy, host1.clone().try_into().ok());
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");

        let watch_responder = expect_watch_request(&mut exec, &mut server);
        // Respond with the same active host, and a different inactive host.
        let host2 =
            example_host(HostId(2), /* active= */ false, /* discoverable= */ false);
        let _ = watch_responder.send(&[host1.clone(), host2]).unwrap();

        // No HostWatcher stream item because the active host has not changed.
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");

        let watch_responder = expect_watch_request(&mut exec, &mut server);
        // Same active host changes - but not in a relevant way.
        host1.discovering = Some(false);
        host1.local_name = Some("123".to_string());
        let _ = watch_responder.send(&[host1]).unwrap();

        // No HostWatcher stream item because the discoverable of the active host hasn't changed.
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");
    }

    #[fuchsia::test]
    fn update_with_active_host_discoverable_change_yields_item() {
        let mut exec = fasync::TestExecutor::new();

        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<sys::HostWatcherMarker>();
        let mut host1 =
            example_host(HostId(1), /* active= */ true, /* discoverable= */ false);
        // HostWatcher starts off with a tracked active, non-discoverable host.
        let mut watcher = HostWatcher::new_with_state(proxy, host1.clone().try_into().ok());
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");

        let watch_responder = expect_watch_request(&mut exec, &mut server);
        // The same active host becomes discoverable.
        host1.discoverable = Some(true);
        let _ = watch_responder.send(&[host1.clone()]).unwrap();

        let item = exec.run_until_stalled(&mut watcher.next()).expect("host update");
        assert_matches!(item, Some(Ok(HostEvent::Discoverable(true))));
    }

    #[fuchsia::test]
    fn update_with_new_active_host_yields_item() {
        let mut exec = fasync::TestExecutor::new();

        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<sys::HostWatcherMarker>();
        let host1 =
            example_host(HostId(1), /* active= */ true, /* discoverable= */ false);
        // HostWatcher starts off with a tracked active, non-discoverable host.
        let mut watcher = HostWatcher::new_with_state(proxy, host1.clone().try_into().ok());
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");

        // Receive an update about a new active, non-discoverable host.
        let watch_responder = expect_watch_request(&mut exec, &mut server);
        let host2 =
            example_host(HostId(2), /* active= */ true, /* discoverable= */ false);
        let _ = watch_responder.send(&[host2]).unwrap();

        let item = exec.run_until_stalled(&mut watcher.next()).expect("host update");
        assert_matches!(item, Some(Ok(HostEvent::NewActiveHost { discoverable: false })));

        // Receive an update about a new active, discoverable host.
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");
        let watch_responder = expect_watch_request(&mut exec, &mut server);
        let host3 = example_host(HostId(3), /* active= */ true, /* discoverable= */ true);
        let _ = watch_responder.send(&[host3]).unwrap();

        let item = exec.run_until_stalled(&mut watcher.next()).expect("host update");
        assert_matches!(item, Some(Ok(HostEvent::NewActiveHost { discoverable: true })));
    }

    #[fuchsia::test]
    fn invalidly_formatted_host_returns_error_stream_item() {
        let mut exec = fasync::TestExecutor::new();

        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<sys::HostWatcherMarker>();
        let mut watcher = HostWatcher::new(proxy);
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");
        assert!(!watcher.is_terminated());

        let watch_responder = expect_watch_request(&mut exec, &mut server);
        // HostInfo is missing a bunch of mandatory fields.
        let invalid_host =
            sys::HostInfo { id: Some(HostId(12).into()), active: Some(true), ..Default::default() };
        let _ = watch_responder.send(&[invalid_host]).unwrap();

        let item = exec.run_until_stalled(&mut watcher.next()).expect("host watcher update");
        assert_matches!(item, Some(Err(_)));
        assert!(!watcher.is_terminated());
    }

    #[fuchsia::test]
    fn closing_fidl_server_terminates_host_watcher() {
        let mut exec = fasync::TestExecutor::new();

        let (proxy, mut server) =
            fidl::endpoints::create_proxy_and_stream::<sys::HostWatcherMarker>();
        let mut watcher = HostWatcher::new(proxy);
        let _ = exec.run_until_stalled(&mut watcher.next()).expect_pending("No updates");
        assert!(!watcher.is_terminated());

        let watch_responder = expect_watch_request(&mut exec, &mut server);
        let _ = watch_responder.send(&[]).unwrap();

        // The upstream `HostWatcher` protocol server disconnects. This should result in a FIDL
        // error propagated to the stream. The next time the `HostWatcher` stream is polled, it
        // should detect closure and terminate.
        drop(server);

        let item = exec.run_until_stalled(&mut watcher.next()).expect("host watcher FIDL error");
        assert_matches!(item, Some(Err(_)));
        assert!(!watcher.is_terminated());

        let item = exec.run_until_stalled(&mut watcher.next()).expect("host watcher termination");
        assert_matches!(item, None);
        assert!(watcher.is_terminated());
    }
}
