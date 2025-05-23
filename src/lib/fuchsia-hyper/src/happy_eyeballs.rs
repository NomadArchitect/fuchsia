// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::connect_and_bind_device;
use fuchsia_async::{net, MonotonicInstant, Timer};

use futures::future::{Fuse, Future, FutureExt};
use futures::stream::{FuturesUnordered, Stream};
use itertools::{Interleave, Itertools};
use pin_project::pin_project;
use std::cmp::{max, min};
use std::io;
use std::iter::Peekable;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::vec::IntoIter;

pub(crate) trait SocketConnector {
    type Connection;
    type Fut: Future<Output = io::Result<Self::Connection>>;

    fn connect(&mut self, addr: SocketAddr, bind_device: Option<&str>) -> io::Result<Self::Fut>;
}

pub(crate) struct RealSocketConnector;
impl SocketConnector for RealSocketConnector {
    type Connection = net::TcpStream;
    type Fut = net::TcpConnector;

    fn connect(&mut self, addr: SocketAddr, bind_device: Option<&str>) -> io::Result<Self::Fut> {
        connect_and_bind_device(addr, bind_device)
    }
}

/// Recommended minimum time between connection attempts according to [RFC8305] Happy Eyeballs
/// version 2 §5p3.
///
/// [RFC8305]: https://tools.ietf.org/html/rfc8305#section-5
pub(crate) const RECOMMENDED_MIN_CONN_ATT_DELAY: zx::MonotonicDuration =
    zx::MonotonicDuration::from_millis(100);

/// Recommended time between connection attempts according to [RFC8305] Happy Eyeballs
/// version 2 §5p2.
///
/// [RFC8305]: https://tools.ietf.org/html/rfc8305#section-5
pub(crate) const RECOMMENDED_CONN_ATT_DELAY: zx::MonotonicDuration =
    zx::MonotonicDuration::from_millis(250);

/// Minimum time between connection attempts according to [RFC8305] Happy Eyeballs version 2 §5p3.
///
/// [RFC8305]: https://tools.ietf.org/html/rfc8305#section-5
const ABS_MIN_CONN_ATT_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(10);

/// Recommended maximum time between connection attempts according to [RFC8305] Happy Eyeballs
/// version 2 §5p3.
///
/// [RFC8305]: https://tools.ietf.org/html/rfc8305#section-5
const ABS_MAX_CONN_ATT_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);

/// happy_eyeballs supplies a partial implementation of [RFC8305] Happy Eyeballs version 2,
/// including §4p4 and the "simple implementation" described in §5p2.
///
/// The delay value provided in `min_conn_att_delay` is used to reduce latency between connections
/// if no active connections are being managed. The delay value in `conn_att_delay` is the normal
/// interval between initiating connections. `RECOMMENDED_MIN_CONN_ATT_DELAY` and
/// `RECOMMENDED_CONN_ATT_DELAY` (respectively) are suggested for use for these values.
///
/// `min_conn_att_delay` is converted to be at least 10 milliseconds and at most the value supplied
/// in `conn_att_delay` -- which is converted to be at least 10 milliseconds, and at most 2
/// seconds.
///
/// [RFC8305]: https://tools.ietf.org/html/rfc8305
pub(crate) fn happy_eyeballs<A, C>(
    addrs: A,
    connector: C,
    min_conn_att_delay: zx::MonotonicDuration,
    conn_att_delay: zx::MonotonicDuration,
    bind_device: Option<&str>,
) -> HappyEyeballs<C>
where
    A: IntoIterator<Item = SocketAddr>,
    C: SocketConnector,
{
    // RFC8305§4p4: Interleave the address families.
    let mut addrs = addrs.into_iter().peekable();

    let v4first: bool = match addrs.peek() {
        Some(addr) => addr.is_ipv4(),
        None => false,
    };

    // TODO(https://fxbug.dev/42147654): Implement an iterator over addrs such that we avoid allocating two
    // new Vecs here.
    let (v4, v6): (Vec<_>, Vec<_>) = addrs.partition(|a| a.is_ipv4());
    let addrs = match v4first {
        true => v4.into_iter().interleave(v6.into_iter()),
        false => v6.into_iter().interleave(v4.into_iter()),
    };

    // Clamp supplied values within spec-defined absolutes and ensure that the minimum delay
    // interval is <= the configured regular interval.
    let conn_att_delay = min(max(ABS_MIN_CONN_ATT_DELAY, conn_att_delay), ABS_MAX_CONN_ATT_DELAY);
    let min_conn_att_delay = min(max(ABS_MIN_CONN_ATT_DELAY, min_conn_att_delay), conn_att_delay);

    HappyEyeballs {
        inner: Inner::new(
            addrs.peekable(),
            connector,
            min_conn_att_delay,
            conn_att_delay,
            bind_device,
        )
        .fuse(),
    }
}

#[pin_project]
pub(crate) struct HappyEyeballs<C>
where
    C: SocketConnector,
{
    // FIXME(https://github.com/rust-lang/futures-rs/issues/2327) After we connect, we should close
    // any pending connections to make sure we're not using excess resources. Unfortunately
    // `FuturesUnordered` doesn't provide a direct method for directly dropping pending resources.
    #[pin]
    inner: Fuse<Inner<C>>,
}

impl<C: SocketConnector> Future for HappyEyeballs<C> {
    type Output = Result<C::Connection, io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

#[pin_project]
struct Inner<C>
where
    C: SocketConnector,
{
    addrs: Addrs<C>,
    #[pin]
    connection_futs: FuturesUnordered<C::Fut>,
    #[pin]
    timer: Timer,
    min_conn_att_delay: zx::MonotonicDuration,
    conn_att_delay: zx::MonotonicDuration,
    last_wake: MonotonicInstant,
    next_wake: MonotonicInstant,
}

impl<C: SocketConnector> Inner<C> {
    fn new(
        addrs: Peekable<Interleave<IntoIter<SocketAddr>, IntoIter<SocketAddr>>>,
        connector: C,
        min_conn_att_delay: zx::MonotonicDuration,
        conn_att_delay: zx::MonotonicDuration,
        bind_device: Option<&str>,
    ) -> Self {
        let last_wake = MonotonicInstant::now();
        let first_deadline = MonotonicInstant::from_nanos(
            last_wake.into_nanos().saturating_add(conn_att_delay.into_nanos()),
        );

        let mut inner = Inner {
            addrs: Addrs {
                addrs,
                connector,
                bind_device: bind_device.map(str::to_string),
                err: None,
            },
            connection_futs: FuturesUnordered::new(),
            min_conn_att_delay,
            conn_att_delay,
            last_wake,
            next_wake: first_deadline,
            timer: Timer::new(first_deadline),
        };

        // Ensure that we've enqueued something to do when we're first polled.
        if let Some(conn_fut) = inner.addrs.next_conn() {
            inner.connection_futs.push(conn_fut);
        }

        inner
    }

    // This is logically part of the Future implementation on this type; it polls the
    // connection_futs collection in a loop. The loop terminates when a ready connection is found,
    // when no more connection futures are ready, or when all futures are tried and none yielded a
    // successful connection. The loop only proceeds when a future was ready and yielded an error
    // instead of a connection. This is done to drain our ready futures in a single poll cycle.
    fn drain_ready_futures(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<C::Connection, io::Error>> {
        let mut this = self.project();
        loop {
            let r = this.connection_futs.as_mut().poll_next(cx);
            match r {
                // Return the successful connection.
                Poll::Ready(Some(Ok(conn))) => {
                    break Poll::Ready(Ok(conn));
                }

                // Stash the last error state to return to the caller when we exhaust the
                // address list. There may be more ready connectors in our collection, so we
                // poll again after this to consume all futures that have reached some final
                // disposition.
                Poll::Ready(Some(Err(err))) => {
                    this.addrs.err = Some(err);
                }

                // No managed connection futures are ready; there's nothing more to poll.
                Poll::Pending => {
                    break Poll::Pending;
                }

                // No connection futures are being managed.
                Poll::Ready(None) => {
                    match this.addrs.peek() {
                        None => {
                            // We don't have any more addresses to try to connect to, and we
                            // aren't waiting on any additional addresses. Return the last
                            // error; if it doesn't exist, we were supplied an empty address
                            // list.
                            break Poll::Ready(Err(this.addrs.err.take().unwrap_or_else(|| {
                                io::Error::new(io::ErrorKind::InvalidInput, "no addresses supplied")
                            })));
                        }
                        Some(_) => {
                            // In this case, we do have more addresses to try. As a small
                            // optimization, we try to initiate the next connection at the
                            // earliest allowable moment by constraining our timer to fire
                            // after the minimum interval past its last fire time.
                            let next_deadline = MonotonicInstant::from_nanos(
                                this.last_wake
                                    .into_nanos()
                                    .saturating_add(this.min_conn_att_delay.into_nanos()),
                            );
                            if next_deadline < *this.next_wake {
                                *this.next_wake = next_deadline;

                                // N.B. This timer change is safe because the timer is
                                // unconditionally polled when we exit the match.
                                this.timer.reset(next_deadline);
                            }

                            break Poll::Pending;
                        }
                    }
                }
            }
        }
    }
}

impl<C: SocketConnector> Future for Inner<C> {
    type Output = Result<C::Connection, io::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Both FuturesUnordered and the fuchsia_async::Timer require polling when they are mutated
        // so that they are registered with a waker. Unless draining ready futures from our
        // collection yields a terminal result, the timer is always polled in this loop. This loop
        // exists to poll the future collection again in case a timer fired and resulted in adding
        // a new future.
        loop {
            // Draining the futures will only result in returning ready on terminal success or
            // failure; any other state yields pending.
            match self.as_mut().drain_ready_futures(cx) {
                Poll::Ready(f) => return Poll::Ready(f),
                Poll::Pending => {}
            }

            let mut this = self.as_mut().project();
            match this.timer.as_mut().poll(cx) {
                // On this arm, there were no requests to handle, and the poll has guaranteed
                // any timer mutation is registered with the executor. We're pending.
                Poll::Pending => {
                    break Poll::Pending;
                }
                Poll::Ready(()) => {
                    if let Some(conn_fut) = this.addrs.next_conn() {
                        // The timer fired and new connections are available. Schedule this
                        // connection to be tried and re-arm the timer to fire again after the
                        // provided connection attempt interval. The timer requires polling, but
                        // since we continue the loop in this arm, we're guaranteed that the
                        // re-armed timer will be polled if there's not a success communicated
                        // through the connection_futs first.
                        this.connection_futs.push(conn_fut);

                        // Only re-arm the timer if we have another address to try.
                        if this.addrs.peek().is_some() {
                            *this.last_wake = MonotonicInstant::now();
                            let next_deadline = MonotonicInstant::from_nanos(
                                this.last_wake
                                    .into_nanos()
                                    .saturating_add(this.conn_att_delay.into_nanos()),
                            );
                            *this.next_wake = next_deadline;
                            this.timer.reset(next_deadline);
                        }
                    } else {
                        break Poll::Pending;
                    }
                }
            }
        }
    }
}

type RawAddrs =
    Peekable<Interleave<std::vec::IntoIter<SocketAddr>, std::vec::IntoIter<SocketAddr>>>;

struct Addrs<C> {
    addrs: RawAddrs,
    connector: C,
    bind_device: Option<String>,
    err: Option<io::Error>,
}

impl<C: SocketConnector> Addrs<C> {
    // Try to connect to the address, or cache a possible error to return after exhausting the
    // entire address list.
    fn next_conn(&mut self) -> Option<C::Fut> {
        while let Some(addr) = self.addrs.next() {
            match self.connector.connect(addr, self.bind_device.as_deref()) {
                Ok(c) => {
                    return Some(c);
                }
                Err(err) => {
                    self.err = Some(err);
                }
            }
        }

        None
    }
}

impl<C> Deref for Addrs<C> {
    type Target = RawAddrs;
    fn deref(&self) -> &Self::Target {
        &self.addrs
    }
}

impl<C> DerefMut for Addrs<C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.addrs
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use assert_matches::assert_matches;
    use fuchsia_async::{self as fasync};
    use fuchsia_sync::Mutex;

    use std::collections::HashMap;
    use std::fmt::Debug;
    use std::io::{Error, ErrorKind};
    use std::iter::once;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::pin::pin;
    use std::sync::Arc;
    use test_case::test_case;

    #[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
    enum Class {
        Connectable,
        DelayedConnectable { delay: zx::MonotonicDuration },
        NotListening,
        Blackholed,
    }

    #[allow(dead_code)] // TODO(https://fxbug.dev/330168307)
    #[derive(Debug, PartialEq, Eq)]
    struct AddrClass {
        addr: SocketAddr,
        class: Class,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Connecting { addr: SocketAddr, class: Class, bind_device: Option<String> },
        DelayFinished { addr: SocketAddr },
    }

    // TestEnvConnector is a connector that is able to simulate various different network
    // conditions, including local (immediately ready) connections, connections that complete after
    // some variable latency, blackholed routes, and invalid destinations. Furthermore, the
    // connector records events driven through it such that tests can assert a deterministic
    // ordering.
    //
    // This connector only permits test designs that yield exactly 0 or 1 successful "connections".
    #[derive(Clone)]
    struct TestEnvConnector {
        inner: Arc<Mutex<InnerConnector>>,
    }

    struct InnerConnector {
        addrclass: HashMap<SocketAddr, Class>,
        events: Vec<Event>,
        server_conn: Option<SocketAddr>,
    }

    impl TestEnvConnector {
        fn new(server_conn: Option<SocketAddr>) -> Self {
            TestEnvConnector {
                inner: Arc::new(Mutex::new(InnerConnector {
                    addrclass: HashMap::new(),
                    events: Vec::new(),
                    server_conn,
                })),
            }
        }

        fn add_classified_addrs(self, class: Class, addrs: Vec<SocketAddr>) -> TestEnvConnector {
            self.inner.lock().addrclass.extend(addrs.into_iter().map(|a| (a, class)));
            self
        }

        fn take_events(&mut self) -> Vec<Event> {
            self.inner.lock().events.drain(..).collect()
        }
    }

    impl SocketConnector for TestEnvConnector {
        type Connection = SocketAddr;
        type Fut = futures::future::LocalBoxFuture<'static, io::Result<Self::Connection>>;

        fn connect(
            &mut self,
            addr: SocketAddr,
            bind_device: Option<&str>,
        ) -> io::Result<Self::Fut> {
            let inner = self.inner.clone();
            let bind_device = bind_device.map(str::to_string);
            Ok(async move {
                let class = {
                    let mut inner = inner.lock();
                    let class = *inner.addrclass.get(&addr).unwrap_or_else(|| {
                        panic!("expected to resolve class for address {:#}", addr)
                    });
                    inner.events.push(Event::Connecting { addr, class, bind_device });
                    class
                };

                match class {
                    Class::Connectable => Ok(inner
                        .lock()
                        .server_conn
                        .take()
                        .expect("that the pseudo-connection wasn't already acquired")),
                    Class::DelayedConnectable { delay } => {
                        let () = Timer::new(MonotonicInstant::after(delay)).await;

                        inner.lock().events.push(Event::DelayFinished { addr });

                        Ok(inner
                            .lock()
                            .server_conn
                            .take()
                            .expect("that the delayed pseudo-connection wasn't already acquired"))
                    }
                    Class::NotListening => {
                        Err(Error::new(ErrorKind::ConnectionRefused, "can't connect"))
                    }
                    Class::Blackholed => {
                        let () = futures::future::pending().await;
                        unreachable!()
                    }
                }
            }
            .boxed_local())
        }
    }

    fn next_event<F>(
        executor: &mut fasync::TestExecutor,
        fut: &mut F,
        duration: zx::MonotonicDuration,
    ) -> Poll<F::Output>
    where
        F: Future + Unpin,
        <F as Future>::Output: Debug,
    {
        if duration == zx::MonotonicDuration::from_millis(0) {
            assert!(!executor.wake_expired_timers());
        } else {
            // Advance time right before the timer is supposed to fire, and make sure it does not.
            let () = executor
                .set_fake_time(executor.now() + duration - zx::MonotonicDuration::from_millis(1));
            assert!(!executor.wake_expired_timers());
            assert_matches!(executor.run_until_stalled(fut), Poll::Pending);

            // Advance time to when the timer should fire and make sure it does.
            let () = executor.set_fake_time(executor.now() + zx::MonotonicDuration::from_millis(1));
            assert!(executor.wake_expired_timers());
        }

        executor.run_until_stalled(fut)
    }

    // Ensure `happy_eyeballs` errors out if no addresses are passed in.
    #[test]
    fn test_no_addrs_error() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let mut connector = TestEnvConnector::new(None);

        let mut fut = pin!(happy_eyeballs(
            vec![],
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            None,
        ));

        // Connect to the service. This should fail on the first poll cycle.
        assert_matches!(
            executor.run_until_stalled(&mut fut),
            Poll::Ready(Err(err)) if err.kind() == io::ErrorKind::InvalidInput);
        assert_eq!(connector.take_events(), vec![]);
    }

    // Test we error out if all addresses fail.
    #[test_case(
        vec![
            (Ipv4Addr::LOCALHOST, 8001).into(),
            (Ipv4Addr::LOCALHOST, 8002).into(),
            (Ipv4Addr::LOCALHOST, 8003).into(),
        ]
        ; "v4 not listening"
    )]
    #[test_case(
        vec![
            (Ipv6Addr::LOCALHOST, 8001).into(),
            (Ipv6Addr::LOCALHOST, 8002).into(),
            (Ipv6Addr::LOCALHOST, 8003).into(),
        ]
        ; "v6 not listening"
    )]
    fn test_all_not_listening_eventually_fails(fail_addrs: Vec<SocketAddr>) {
        let mut executor = fasync::TestExecutor::new_with_fake_time();

        let mut connector = TestEnvConnector::new(None)
            .add_classified_addrs(Class::NotListening, fail_addrs.clone());

        let mut fut = pin!(happy_eyeballs(
            fail_addrs.clone(),
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            Some("expected_device"),
        ));

        let mut fail_addrs = fail_addrs.into_iter();

        // Pop off the last addrs since we handle them differently.
        let last_fail_addr = fail_addrs.next_back().unwrap();

        // Trigger all the failing polls.
        for (delay, expected_event) in fail_addrs.enumerate().map(|(i, addr)| {
            let delay = if i == 0 {
                zx::MonotonicDuration::from_millis(0)
            } else {
                RECOMMENDED_MIN_CONN_ATT_DELAY
            };
            (
                delay,
                Event::Connecting {
                    addr,
                    class: Class::NotListening,
                    bind_device: Some("expected_device".into()),
                },
            )
        }) {
            assert_matches::assert_matches!(
                next_event(&mut executor, &mut fut, delay),
                Poll::Pending
            );
            assert_eq!(connector.take_events(), vec![expected_event]);
        }

        // Advance the time to after the min retry timeout, but before the max retry timeout.
        // This should succeed because we reset the timer.
        assert_matches::assert_matches!(
            next_event(&mut executor, &mut fut, RECOMMENDED_MIN_CONN_ATT_DELAY),
            Poll::Ready(Err(err)) if err.kind() == io::ErrorKind::ConnectionRefused
        );
        assert_eq!(
            connector.take_events(),
            vec![Event::Connecting {
                addr: last_fail_addr,
                class: Class::NotListening,
                bind_device: Some("expected_device".into())
            }]
        );
    }

    // Test that happy_eyeballs never returns if all addresses are blackholed.
    #[test_case(
        vec![
            (Ipv4Addr::LOCALHOST, 8001).into(),
            (Ipv4Addr::LOCALHOST, 8002).into(),
            (Ipv4Addr::LOCALHOST, 8003).into(),
        ]
        ; "v4 blackholed"
    )]
    #[test_case(
        vec![
            (Ipv6Addr::LOCALHOST, 8001).into(),
            (Ipv6Addr::LOCALHOST, 8002).into(),
            (Ipv6Addr::LOCALHOST, 8003).into(),
        ]
        ; "v6 blackholed"
    )]
    fn test_all_blackholed_never_succeeds(fail_addrs: Vec<SocketAddr>) {
        let mut executor = fasync::TestExecutor::new_with_fake_time();

        let mut connector =
            TestEnvConnector::new(None).add_classified_addrs(Class::Blackholed, fail_addrs.clone());

        let mut fut = pin!(happy_eyeballs(
            fail_addrs.clone(),
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            Some("bind_device"),
        ));

        // Trigger all the failing polls.
        for (delay, expected_event) in fail_addrs.iter().enumerate().map(|(i, addr)| {
            let delay = if i == 0 {
                zx::MonotonicDuration::from_millis(0)
            } else {
                RECOMMENDED_CONN_ATT_DELAY
            };
            (
                delay,
                Event::Connecting {
                    addr: *addr,
                    class: Class::Blackholed,
                    bind_device: Some("bind_device".into()),
                },
            )
        }) {
            assert_matches::assert_matches!(
                next_event(&mut executor, &mut fut, delay),
                Poll::Pending
            );
            assert_eq!(connector.take_events(), vec![expected_event]);
        }

        // Advance the time to after the min retry timeout, but before the max retry timeout and
        // validate that no events are observed. Note we don't use next_event here because the
        // timer is not re-armed when there are no more addresses to try.
        let () = executor.set_fake_time(executor.now() + RECOMMENDED_CONN_ATT_DELAY);
        assert!(!executor.wake_expired_timers());
        assert_matches::assert_matches!(executor.run_until_stalled(&mut fut), Poll::Pending);
        assert_eq!(connector.take_events(), vec![]);
    }

    // This test validates that a single V4 or V6 endpoint can be connected.
    #[test_case((Ipv4Addr::LOCALHOST, 8000).into(); "v4")]
    #[test_case((Ipv6Addr::LOCALHOST, 8000).into(); "v6")]
    fn test_single_valid_address(server_addr: SocketAddr) {
        let mut executor = fasync::TestExecutor::new_with_fake_time();

        let mut connector = TestEnvConnector::new(Some(server_addr))
            .add_classified_addrs(Class::Connectable, vec![server_addr]);

        let mut fut = pin!(happy_eyeballs(
            vec![server_addr],
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            Some("single_valid"),
        ));

        // Connect to the service. This succeeds because the address is good.
        assert_matches::assert_matches!(
            executor.run_until_stalled(&mut fut),
            Poll::Ready(Ok(a)) if a == server_addr
        );
        assert_eq!(
            connector.take_events(),
            vec![Event::Connecting {
                addr: server_addr,
                class: Class::Connectable,
                bind_device: Some("single_valid".into())
            }]
        );
    }

    // This test checks that, in the presence of unreachable destinations, a successful address
    // provided at the head of the list will succeed.
    #[test_case((Ipv4Addr::LOCALHOST, 8000).into(); "v4")]
    #[test_case((Ipv6Addr::LOCALHOST, 8000).into(); "v6")]
    fn test_address_works_in_bad_network(server_addr: SocketAddr) {
        let mut executor = fasync::TestExecutor::new_with_fake_time();

        let nonlistening_addr = (Ipv4Addr::LOCALHOST, 8001).into();
        let blackhole_addr = (Ipv6Addr::LOCALHOST, 8002).into();

        let mut connector = TestEnvConnector::new(Some(server_addr))
            .add_classified_addrs(Class::Connectable, vec![server_addr])
            .add_classified_addrs(Class::NotListening, vec![nonlistening_addr])
            .add_classified_addrs(Class::Blackholed, vec![blackhole_addr]);

        // Try to connect to each address. Only the first should be checked.
        let mut fut = pin!(happy_eyeballs(
            vec![server_addr, nonlistening_addr, blackhole_addr],
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            Some("bad_network"),
        ));

        // Connect to the service. This succeeds because the first address is good.
        assert_matches::assert_matches!(
            executor.run_until_stalled(&mut fut),
            Poll::Ready(Ok(a)) if a == server_addr
        );
        assert_eq!(
            connector.take_events(),
            vec![Event::Connecting {
                addr: server_addr,
                class: Class::Connectable,
                bind_device: Some("bad_network".into()),
            }]
        );
    }

    // This test checks that, given two V4 or V6 addresses, when the first fails to connect, a
    // second fallback address can connect, and that is also the order of operations.
    #[test_case(
        (Ipv4Addr::LOCALHOST, 8000),
        Class::NotListening,
        vec![
            (Ipv4Addr::LOCALHOST, 8001),
            (Ipv4Addr::LOCALHOST, 8002),
            (Ipv4Addr::LOCALHOST, 8003),
        ],
        RECOMMENDED_MIN_CONN_ATT_DELAY
        ; "v4 not listening"
    )]
    #[test_case(
        (Ipv6Addr::LOCALHOST, 8000),
        Class::NotListening,
        vec![
            (Ipv6Addr::LOCALHOST, 8001),
            (Ipv6Addr::LOCALHOST, 8002),
            (Ipv6Addr::LOCALHOST, 8003),
        ],
        RECOMMENDED_MIN_CONN_ATT_DELAY
        ; "v6 not listening"
    )]
    #[test_case(
        (Ipv4Addr::LOCALHOST, 8000),
        Class::Blackholed,
        vec![
            (Ipv4Addr::LOCALHOST, 8001),
            (Ipv4Addr::LOCALHOST, 8002),
            (Ipv4Addr::LOCALHOST, 8003),
        ],
        RECOMMENDED_CONN_ATT_DELAY
        ; "v4 blackholed"
    )]
    #[test_case(
        (Ipv6Addr::LOCALHOST, 8000),
        Class::Blackholed,
        vec![
            (Ipv6Addr::LOCALHOST, 8001),
            (Ipv6Addr::LOCALHOST, 8002),
            (Ipv6Addr::LOCALHOST, 8003),
        ],
        RECOMMENDED_CONN_ATT_DELAY
        ; "v6 blackholed"
    )]
    fn test_fallback<SA, FA>(
        server_addr: SA,
        fail_class: Class,
        fail_addrs: Vec<FA>,
        delay: zx::MonotonicDuration,
    ) where
        SA: Into<SocketAddr>,
        FA: Into<SocketAddr>,
    {
        let server_addr = server_addr.into();
        let fail_addrs = fail_addrs.into_iter().map(|a| a.into()).collect::<Vec<_>>();

        let mut executor = fasync::TestExecutor::new_with_fake_time();

        let mut connector = TestEnvConnector::new(Some(server_addr))
            .add_classified_addrs(Class::Connectable, vec![server_addr])
            .add_classified_addrs(fail_class, fail_addrs.clone());

        let mut fut = pin!(happy_eyeballs(
            fail_addrs.iter().cloned().chain(once(server_addr)),
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            Some("test_fallback"),
        ));

        // Trigger all the failing polls.
        for (delay, expected_event) in fail_addrs.iter().enumerate().map(|(i, addr)| {
            let delay = if i == 0 { zx::MonotonicDuration::from_millis(0) } else { delay };
            (
                delay,
                Event::Connecting {
                    addr: *addr,
                    class: fail_class,
                    bind_device: Some("test_fallback".into()),
                },
            )
        }) {
            assert_matches::assert_matches!(
                next_event(&mut executor, &mut fut, delay),
                Poll::Pending
            );
            assert_eq!(connector.take_events(), vec![expected_event]);
        }

        // Advance the time to after the min retry timeout, but before the max retry timeout.
        // This should succeed because we reset the timer.
        assert_matches::assert_matches!(
            next_event(&mut executor, &mut fut, delay),
            Poll::Ready(Ok(a)) if a == server_addr
        );
        assert_eq!(
            connector.take_events(),
            vec![Event::Connecting {
                addr: server_addr,
                class: Class::Connectable,
                bind_device: Some("test_fallback".into())
            }]
        );
    }

    // This test checks that, given two V4/V6 addresses, when the first connection would exceed the
    // supplied timeout, a second reachable address is used.
    #[test_case(
        (Ipv4Addr::LOCALHOST, 8000).into(),
        vec![
            (Ipv4Addr::LOCALHOST, 8001).into(),
            (Ipv4Addr::LOCALHOST, 8002).into(),
            (Ipv4Addr::LOCALHOST, 8003).into(),
        ]
        ; "v4"
    )]
    #[test_case(
        (Ipv6Addr::LOCALHOST, 8000).into(),
        vec![
            (Ipv6Addr::LOCALHOST, 8001).into(),
            (Ipv6Addr::LOCALHOST, 8002).into(),
            (Ipv6Addr::LOCALHOST, 8003).into(),
        ]
        ; "v6"
    )]
    fn test_fallback_blackholed(server_addr: SocketAddr, fail_addrs: Vec<SocketAddr>) {
        let mut executor = fasync::TestExecutor::new_with_fake_time();

        let mut connector = TestEnvConnector::new(Some(server_addr))
            .add_classified_addrs(Class::Connectable, vec![server_addr])
            .add_classified_addrs(Class::Blackholed, fail_addrs.clone());

        let mut fut = pin!(happy_eyeballs(
            fail_addrs.iter().cloned().chain(once(server_addr)),
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            Some("blackhole"),
        ));

        // Trigger all the failing polls.
        for (delay, expected_event) in fail_addrs.iter().enumerate().map(|(i, addr)| {
            let delay = if i == 0 {
                zx::MonotonicDuration::from_millis(0)
            } else {
                RECOMMENDED_CONN_ATT_DELAY
            };
            (
                delay,
                Event::Connecting {
                    addr: *addr,
                    class: Class::Blackholed,
                    bind_device: Some("blackhole".into()),
                },
            )
        }) {
            assert_matches::assert_matches!(
                next_event(&mut executor, &mut fut, delay),
                Poll::Pending
            );
            assert_eq!(connector.take_events(), vec![expected_event]);
        }

        // Advance the time after the retry timeout. This should succeed.
        assert_matches::assert_matches!(
            next_event(&mut executor, &mut fut, RECOMMENDED_CONN_ATT_DELAY),
            Poll::Ready(Ok(a)) if a == server_addr
        );
        assert_eq!(
            connector.take_events(),
            vec![Event::Connecting {
                addr: server_addr,
                class: Class::Connectable,
                bind_device: Some("blackhole".into())
            }]
        );
    }

    // This test validates that, across IP versions and reasons for failure, the presence of a
    // reachable address at the end will succeed.
    #[test]
    fn test_fallback_crossproto_crosscause() {
        let server_addr = (Ipv4Addr::LOCALHOST, 8000).into();
        let nl_v4 = (Ipv4Addr::LOCALHOST, 8001).into();
        let nl_v6 = (Ipv6Addr::LOCALHOST, 8002).into();
        let bh_v4 = (Ipv4Addr::LOCALHOST, 8003).into();
        let bh_v6 = (Ipv6Addr::LOCALHOST, 8004).into();

        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let () = executor.set_fake_time(MonotonicInstant::from_nanos(0));

        let mut connector = TestEnvConnector::new(Some(server_addr))
            .add_classified_addrs(Class::Connectable, vec![server_addr])
            .add_classified_addrs(Class::NotListening, vec![nl_v4, nl_v6])
            .add_classified_addrs(Class::Blackholed, vec![bh_v4, bh_v6]);

        let mut fut = pin!(happy_eyeballs(
            vec![nl_v4, bh_v6, bh_v4, nl_v6, server_addr],
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            Some("crossproto"),
        ));

        // Trigger all the failing polls.
        for (delay, expected_event) in vec![
            (
                zx::MonotonicDuration::from_millis(0),
                Event::Connecting {
                    addr: nl_v4,
                    class: Class::NotListening,
                    bind_device: Some("crossproto".into()),
                },
            ),
            (
                RECOMMENDED_MIN_CONN_ATT_DELAY,
                Event::Connecting {
                    addr: bh_v6,
                    class: Class::Blackholed,
                    bind_device: Some("crossproto".into()),
                },
            ),
            (
                RECOMMENDED_CONN_ATT_DELAY,
                Event::Connecting {
                    addr: bh_v4,
                    class: Class::Blackholed,
                    bind_device: Some("crossproto".into()),
                },
            ),
            (
                RECOMMENDED_CONN_ATT_DELAY,
                Event::Connecting {
                    addr: nl_v6,
                    class: Class::NotListening,
                    bind_device: Some("crossproto".into()),
                },
            ),
        ] {
            assert_matches::assert_matches!(
                next_event(&mut executor, &mut fut, delay),
                Poll::Pending
            );
            assert_eq!(connector.take_events(), vec![expected_event]);
        }

        // Sleep the max amount, then successfully connect to `server_addr`. We need to sleep the
        // max amount because we have outstanding blackholed connections.
        assert_matches::assert_matches!(
            next_event(&mut executor, &mut fut, RECOMMENDED_CONN_ATT_DELAY),
            Poll::Ready(Ok(a)) if a == server_addr
        );
        assert_eq!(
            connector.take_events(),
            vec![Event::Connecting {
                addr: server_addr,
                class: Class::Connectable,
                bind_device: Some("crossproto".into())
            }]
        );
    }

    // This test validates that, provided a list of addresses in unsorted order, the Happy Eyeballs
    // implementation will interleave the addresses by version, and that this interleaving happens
    // on the basis of the first address.
    #[test_case(true; "v4 first")]
    #[test_case(false; "v6 first")]
    fn test_rfc8305s4p4(ipv4_first: bool) {
        let server_addr = (Ipv4Addr::LOCALHOST, 8000).into();
        let nl_v4 = (Ipv4Addr::LOCALHOST, 8001).into();
        let nl_v6 = (Ipv6Addr::LOCALHOST, 8002).into();
        let bh_v4 = (Ipv4Addr::LOCALHOST, 8003).into();
        let bh_v6 = (Ipv6Addr::LOCALHOST, 8004).into();

        let (conn_addrs, expected_events);
        if ipv4_first {
            conn_addrs = vec![nl_v4, bh_v4, bh_v6, nl_v6];
            expected_events = vec![
                (
                    zx::MonotonicDuration::from_millis(0),
                    Event::Connecting {
                        addr: nl_v4,
                        class: Class::NotListening,
                        bind_device: Some("rfc8305s4p4".into()),
                    },
                ),
                (
                    RECOMMENDED_MIN_CONN_ATT_DELAY,
                    Event::Connecting {
                        addr: bh_v6,
                        class: Class::Blackholed,
                        bind_device: Some("rfc8305s4p4".into()),
                    },
                ),
                (
                    RECOMMENDED_CONN_ATT_DELAY,
                    Event::Connecting {
                        addr: bh_v4,
                        class: Class::Blackholed,
                        bind_device: Some("rfc8305s4p4".into()),
                    },
                ),
                (
                    RECOMMENDED_CONN_ATT_DELAY,
                    Event::Connecting {
                        addr: nl_v6,
                        class: Class::NotListening,
                        bind_device: Some("rfc8305s4p4".into()),
                    },
                ),
            ];
        } else {
            conn_addrs = vec![nl_v6, bh_v6, bh_v4, nl_v4];
            expected_events = vec![
                (
                    zx::MonotonicDuration::from_millis(0),
                    Event::Connecting {
                        addr: nl_v6,
                        class: Class::NotListening,
                        bind_device: Some("rfc8305s4p4".into()),
                    },
                ),
                (
                    RECOMMENDED_MIN_CONN_ATT_DELAY,
                    Event::Connecting {
                        addr: bh_v4,
                        class: Class::Blackholed,
                        bind_device: Some("rfc8305s4p4".into()),
                    },
                ),
                (
                    RECOMMENDED_CONN_ATT_DELAY,
                    Event::Connecting {
                        addr: bh_v6,
                        class: Class::Blackholed,
                        bind_device: Some("rfc8305s4p4".into()),
                    },
                ),
                (
                    RECOMMENDED_CONN_ATT_DELAY,
                    Event::Connecting {
                        addr: nl_v4,
                        class: Class::NotListening,
                        bind_device: Some("rfc8305s4p4".into()),
                    },
                ),
            ];
        };

        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let () = executor.set_fake_time(MonotonicInstant::from_nanos(0));

        // First, test that we'll try to connect to IPv4 first if it's first in the list.
        let mut connector = TestEnvConnector::new(Some(server_addr))
            .add_classified_addrs(Class::Connectable, vec![server_addr])
            .add_classified_addrs(Class::NotListening, vec![nl_v4, nl_v6])
            .add_classified_addrs(Class::Blackholed, vec![bh_v4, bh_v6]);

        let mut fut = pin!(happy_eyeballs(
            conn_addrs.iter().cloned().chain(once(server_addr)),
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            Some("rfc8305s4p4"),
        ));

        // Trigger all the failing polls.
        for (delay, expected_event) in expected_events {
            assert_matches::assert_matches!(
                next_event(&mut executor, &mut fut, delay),
                Poll::Pending
            );
            assert_eq!(connector.take_events(), vec![expected_event]);
        }

        assert_matches::assert_matches!(
            next_event(&mut executor, &mut fut, RECOMMENDED_CONN_ATT_DELAY),
            Poll::Ready(Ok(a)) if a == server_addr
        );
        assert_eq!(
            connector.take_events(),
            vec![Event::Connecting {
                addr: server_addr,
                class: Class::Connectable,
                bind_device: Some("rfc8305s4p4".into())
            }]
        );
    }

    // Test that a latent endpoint can eventually be connected to, even if we've attempted
    // additional connections in the meantime.
    #[test]
    fn test_latent_endpoint() {
        let server_addr = (Ipv4Addr::LOCALHOST, 8000).into();
        let bh_addr = (Ipv4Addr::LOCALHOST, 8001).into();

        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let () = executor.set_fake_time(MonotonicInstant::from_nanos(0));

        let delay = RECOMMENDED_CONN_ATT_DELAY + zx::MonotonicDuration::from_millis(5);

        let mut connector = TestEnvConnector::new(Some(server_addr))
            .add_classified_addrs(Class::DelayedConnectable { delay }, vec![server_addr])
            .add_classified_addrs(Class::Blackholed, vec![bh_addr]);

        let mut fut = pin!(happy_eyeballs(
            vec![server_addr, bh_addr],
            connector.clone(),
            RECOMMENDED_MIN_CONN_ATT_DELAY,
            RECOMMENDED_CONN_ATT_DELAY,
            Some("latent_endpoint"),
        ));

        // Trigger all the failing polls.
        for (delay, expected_event) in vec![
            (
                zx::MonotonicDuration::from_millis(0),
                Event::Connecting {
                    addr: server_addr,
                    class: Class::DelayedConnectable { delay },
                    bind_device: Some("latent_endpoint".into()),
                },
            ),
            (
                RECOMMENDED_CONN_ATT_DELAY,
                Event::Connecting {
                    addr: bh_addr,
                    class: Class::Blackholed,
                    bind_device: Some("latent_endpoint".into()),
                },
            ),
        ] {
            assert_matches::assert_matches!(
                next_event(&mut executor, &mut fut, delay),
                Poll::Pending
            );
            assert_eq!(connector.take_events(), vec![expected_event]);
        }

        // Sleep to when the server should eventually respond.
        assert_matches::assert_matches!(
            next_event(&mut executor, &mut fut, zx::MonotonicDuration::from_millis(5)),
            Poll::Ready(Ok(a)) if a == server_addr
        );
        assert_eq!(connector.take_events(), vec![Event::DelayFinished { addr: server_addr }]);
    }

    // This test validates that:
    //  * out-of-range intervals are clamped to the proper durations,
    //  * the caller-supplied values are used (rather than constants), and
    //  * when no connections are being managed, we rush the next connection.
    #[test]
    fn test_timer_behavior() {
        let server_addr = (Ipv4Addr::LOCALHOST, 8000).into();
        let nl_addr = (Ipv4Addr::LOCALHOST, 8001).into();
        let bh_addr = (Ipv4Addr::LOCALHOST, 8002).into();

        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let () = executor.set_fake_time(MonotonicInstant::from_nanos(0));

        let mut connector = TestEnvConnector::new(Some(server_addr))
            .add_classified_addrs(Class::NotListening, vec![nl_addr])
            .add_classified_addrs(Class::Blackholed, vec![bh_addr])
            .add_classified_addrs(Class::Connectable, vec![server_addr]);

        // The connection order is non-listening, blackhole, connectable. This allows us to
        // measure:
        //  * The non-listening address errored,
        //  * After 1ms, nothing new has happened,
        //  * After 10ms, the blackhole address is scheduled (and we rushed that),
        //  * After 250ms, nothing has happened, and
        //  * After 2s, our good connection is tried (so the 5s interval was clamped).
        let mut fut = pin!(happy_eyeballs(
            vec![nl_addr, bh_addr, server_addr],
            connector.clone(),
            zx::MonotonicDuration::from_millis(1),
            zx::MonotonicDuration::from_seconds(5),
            Some("timer_behavior"),
        ));

        // Walk through all the events that should occur in this setup.
        for (abstime, woke, done, optional_event) in vec![
            (
                zx::MonotonicDuration::from_millis(0),
                false,
                false,
                Some(Event::Connecting {
                    addr: nl_addr,
                    class: Class::NotListening,
                    bind_device: Some("timer_behavior".into()),
                }),
            ),
            (
                zx::MonotonicDuration::from_millis(10),
                true,
                false,
                Some(Event::Connecting {
                    addr: bh_addr,
                    class: Class::Blackholed,
                    bind_device: Some("timer_behavior".into()),
                }),
            ),
            (zx::MonotonicDuration::from_millis(250), false, false, None),
            (
                // N.B. 2010ms is the absolute time for the successful connection because it was
                // scheduled 2s out from the 10ms clamp where the blackholed connection was queued.
                zx::MonotonicDuration::from_millis(2010),
                true,
                true,
                Some(Event::Connecting {
                    addr: server_addr,
                    class: Class::Connectable,
                    bind_device: Some("timer_behavior".into()),
                }),
            ),
        ] {
            let () = executor.set_fake_time(MonotonicInstant::from_nanos(abstime.into_nanos()));

            assert_eq!(executor.wake_expired_timers(), woke);

            let res = executor.run_until_stalled(&mut fut);
            match done {
                false => assert_matches::assert_matches!(res, Poll::Pending),
                true => {
                    assert_matches::assert_matches!(res, Poll::Ready(Ok(a)) if a == server_addr)
                }
            }

            assert_eq!(
                connector.take_events(),
                optional_event.map(|event| vec![event]).unwrap_or_else(Vec::new)
            );
        }
    }
}
