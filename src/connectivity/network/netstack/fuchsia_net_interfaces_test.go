// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package netstack

import (
	"context"
	"errors"
	"fmt"
	"syscall/zx"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/dhcp"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/fidlconv"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/routetypes"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/sync"
	zxtime "go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/time"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/util"

	"fidl/fuchsia/hardware/network"
	fnet "fidl/fuchsia/net"
	"fidl/fuchsia/net/interfaces"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/stack"
)

const testId = 1
const negativeTimeout = 50 * time.Millisecond

func testIpv4Subnet() fnet.Subnet {
	return fnet.Subnet{
		Addr:      fnet.IpAddressWithIpv4(fnet.Ipv4Address{Addr: [4]uint8{1, 2, 3, 4}}),
		PrefixLen: 16,
	}
}

func testIpv4Address() interfaces.Address {
	var addr interfaces.Address
	addr.SetAddr(testIpv4Subnet())
	addr.SetValidUntil(int64(zx.TimensecInfinite))
	addr.SetPreferredLifetimeInfo(interfaces.PreferredLifetimeInfoWithPreferredUntil(int64(zx.TimensecInfinite)))
	addr.SetAssignmentState(interfaces.AddressAssignmentStateAssigned)
	return addr
}

func testProperties() interfaces.Properties {
	var properties interfaces.Properties
	properties.SetId(testId)
	properties.SetName("testif01")
	properties.SetPortClass(interfaces.PortClassWithLoopback(interfaces.Empty{}))
	properties.SetOnline(true)
	properties.SetHasDefaultIpv4Route(true)
	properties.SetHasDefaultIpv6Route(true)
	properties.SetAddresses([]interfaces.Address{})
	return properties
}

// Starts the interface watcher event loop.
//
// Note that this function registers a cleanup function to stop the event
// loop, so tests which need newNetstack to construct a netstack must
// call this function first as the netstack cleanup tasks rely on the
// interface watcher event loop to still be running.
func startEventLoop(t *testing.T) (chan interfaceEvent, chan interfaceWatcherRequest) {
	eventChan := make(chan interfaceEvent)
	watcherChan := make(chan interfaceWatcherRequest)

	ctx, cancel := context.WithCancel(context.Background())
	var wg sync.WaitGroup
	t.Cleanup(func() {
		cancel()
		wg.Wait()
	})
	wg.Add(1)
	go func() {
		defer wg.Done()
		interfaceWatcherEventLoop(ctx, eventChan, watcherChan, &fidlInterfaceWatcherStats{})
	}()

	return eventChan, watcherChan
}

func assertWatchResult(gotEvent interfaces.Event, gotErr error, wantEvent interfaces.Event) error {
	if gotErr != nil {
		return fmt.Errorf("Watch failed: %w", gotErr)
	}
	if diff := cmp.Diff(wantEvent, gotEvent, cmpopts.IgnoreTypes(struct{}{}), cmpopts.EquateEmpty()); diff != "" {
		return fmt.Errorf("(-want +got)\n%s", diff)
	}
	return nil
}

type watchResult struct {
	event interfaces.Event
	err   error
}

type watcherHelper struct {
	*interfaces.WatcherWithCtxInterface
}

func optionsWithFullInterest() interfaces.WatcherOptions {
	var options interfaces.WatcherOptions
	options.SetAddressPropertiesInterest(interfaces.AddressPropertiesInterestValidUntil | interfaces.AddressPropertiesInterestPreferredLifetimeInfo)
	return options
}

func initWatcher(t *testing.T, si *interfaceStateImpl, options interfaces.WatcherOptions) watcherHelper {
	request, watcher, err := interfaces.NewWatcherWithCtxInterfaceRequest()
	if err != nil {
		t.Fatalf("failed to create Watcher protocol channel pair: %s", err)
	}
	if err := si.GetWatcher(context.Background(), options, request); err != nil {
		t.Fatalf("failed to call GetWatcher: %s", err)
	}
	return watcherHelper{
		WatcherWithCtxInterface: watcher,
	}
}

func (w *watcherHelper) expectIdleEvent(t *testing.T) {
	t.Helper()

	event, err := w.Watch(context.Background())
	if err := assertWatchResult(event, err, interfaces.EventWithIdle(interfaces.Empty{})); err != nil {
		t.Fatal(err)
	}
}

// Call `Watch` on the provided watcher, expecting the call to block because
// no `watchResult` is immediately ready.
//
// Note: This function makes a best effort attempt to ensure `Watch` has
// been called before it returning, but it cannot guarantee it. In certain
// execution contexts (i.e. Fuchsia's CQ) it's possible for `negativeTimeout` to
// expire, without the watch goroutine having been scheduled. As such, this
// function should only be used in negative checks (e.g. verifying an event did
// not occur).
func (w *watcherHelper) blockingWatch(t *testing.T, ch chan watchResult) {
	go func() {
		event, err := w.Watch(context.Background())
		ch <- watchResult{
			event: event,
			err:   err,
		}
	}()
	select {
	case got := <-ch:
		t.Fatalf("Watch did not block and completed with: %#v", got)
	case <-zxtime.After(zxtime.Duration(negativeTimeout)):
	}
}

func TestInterfacesWatcherDisallowMultiplePending(t *testing.T) {
	_, watcherChan := startEventLoop(t)
	si := &interfaceStateImpl{watcherChan: watcherChan}

	watcher := initWatcher(t, si, optionsWithFullInterest())
	watcher.expectIdleEvent(t)

	var wg sync.WaitGroup
	defer wg.Wait()

	for i := 0; i < 2; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()

			_, err := watcher.Watch(context.Background())
			var gotErr *zx.Error
			if !(errors.As(err, &gotErr) && gotErr.Status == zx.ErrPeerClosed) {
				t.Errorf("got watcher.Watch() = (_, %s), want %s", err, zx.ErrPeerClosed)
			}
		}()
	}
}

func TestInterfacesWatcherExisting(t *testing.T) {
	eventChan, watcherChan := startEventLoop(t)
	ns, _ := newNetstack(t, netstackTestOptions{interfaceEventChan: eventChan})
	si := &interfaceStateImpl{watcherChan: watcherChan}

	ifs := addNoopEndpoint(t, ns, "")

	watcher := initWatcher(t, si, optionsWithFullInterest())
	defer func() {
		if err := watcher.Close(); err != nil {
			t.Errorf("failed to close watcher: %s", err)
		}
	}()

	event, err := watcher.Watch(context.Background())
	if err := assertWatchResult(event, err, interfaces.EventWithExisting(initialProperties(ifs, ns.name(ifs.nicid)))); err != nil {
		t.Fatal(err)
	}
	watcher.expectIdleEvent(t)
}

func TestInterfacesWatcher(t *testing.T) {
	eventChan, watcherChan := startEventLoop(t)
	ndpDisp := newNDPDispatcher()
	ns, _ := newNetstack(t, netstackTestOptions{
		interfaceEventChan: eventChan,
		ndpDisp:            ndpDisp,
	})
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	ndpDisp.start(ctx)
	si := &interfaceStateImpl{watcherChan: watcherChan}

	// The first watcher will always block, while the second watcher should never block.
	blockingWatcher, nonBlockingWatcher := initWatcher(t, si, optionsWithFullInterest()), initWatcher(t, si, optionsWithFullInterest())
	ch := make(chan watchResult)
	defer func() {
		// NB: The blocking watcher closed at the end of the test instead of deferred as
		// additional assertions are made with it.
		if err := nonBlockingWatcher.Close(); err != nil {
			t.Errorf("failed to close non-blocking watcher: %s", err)
		}
		close(ch)
	}()

	blockingWatcher.expectIdleEvent(t)
	nonBlockingWatcher.expectIdleEvent(t)

	blockingWatcher.blockingWatch(t, ch)

	// Add an interface.
	ifs := addNoopEndpoint(t, ns, "")

	verifyWatchResults := func(wantEvent interfaces.Event) error {
		event, err := nonBlockingWatcher.Watch(context.Background())
		if err := assertWatchResult(event, err, wantEvent); err != nil {
			return fmt.Errorf("non-blocking watcher error: %w", err)
		}

		got := <-ch
		if err := assertWatchResult(got.event, got.err, wantEvent); err != nil {
			return fmt.Errorf("blocking watcher error: %w", err)
		}
		return nil
	}
	if err := verifyWatchResults(interfaces.EventWithAdded(initialProperties(ifs, ns.name(ifs.nicid)))); err != nil {
		t.Fatal(err)
	}

	// Set interface up.
	blockingWatcher.blockingWatch(t, ch)
	if err := ifs.Up(); err != nil {
		t.Fatalf("failed to set interface up: %s", err)
	}
	var id interfaces.Properties
	id.SetId(uint64(ifs.nicid))
	online := id
	online.SetOnline(true)
	if err := verifyWatchResults(interfaces.EventWithChanged(online)); err != nil {
		t.Fatal(err)
	}

	// Add and remove addresses.
	for _, protocolAddr := range []tcpip.ProtocolAddress{
		{
			Protocol: header.IPv4ProtocolNumber,
			AddressWithPrefix: tcpip.AddressWithPrefix{
				Address:   util.Parse("1.2.3.4"),
				PrefixLen: 16,
			},
		},
		{
			Protocol: header.IPv6ProtocolNumber,
			AddressWithPrefix: tcpip.AddressWithPrefix{
				Address:   util.Parse("abcd::1"),
				PrefixLen: 64,
			},
		},
	} {
		blockingWatcher.blockingWatch(t, ch)
		if ok, reason := ifs.addAddress(protocolAddr, stack.AddressProperties{}); !ok {
			t.Fatalf("ifs.addAddress(%s, {}): %s", protocolAddr.AddressWithPrefix, reason)
		}
		addressAdded := id
		var address interfaces.Address
		address.SetAddr(fidlconv.ToNetSubnet(protocolAddr.AddressWithPrefix))
		address.SetValidUntil(int64(zx.TimensecInfinite))
		address.SetPreferredLifetimeInfo(interfaces.PreferredLifetimeInfoWithPreferredUntil(int64(zx.TimensecInfinite)))
		address.SetAssignmentState(interfaces.AddressAssignmentStateAssigned)
		addressAdded.SetAddresses([]interfaces.Address{address})
		if err := verifyWatchResults(interfaces.EventWithChanged(addressAdded)); err != nil {
			t.Fatal(err)
		}

		blockingWatcher.blockingWatch(t, ch)
		if zxStatus := ifs.removeAddress(protocolAddr); zxStatus != zx.ErrOk {
			t.Fatalf("ifs.removeAddress(%s): %s", protocolAddr.AddressWithPrefix, zxStatus)
		}
		addressRemoved := id
		addressRemoved.SetAddresses([]interfaces.Address{})
		if err := verifyWatchResults(interfaces.EventWithChanged(addressRemoved)); err != nil {
			t.Fatal(err)
		}
	}

	// Add a default route.
	blockingWatcher.blockingWatch(t, ch)
	r := defaultV4Route(ifs.nicid, util.Parse("1.2.3.5"))
	if _, err := ns.AddRoute(r, nil /* metric */, false, true /* replaceMatchingGvisorRoutes */, routetypes.GlobalRouteSet()); err != nil {
		t.Fatalf("failed to add default route: %s", err)
	}
	defaultIpv4RouteAdded := id
	defaultIpv4RouteAdded.SetHasDefaultIpv4Route(true)
	if err := verifyWatchResults(interfaces.EventWithChanged(defaultIpv4RouteAdded)); err != nil {
		t.Fatal(err)
	}

	// Remove the default route.
	blockingWatcher.blockingWatch(t, ch)
	_ = ns.DelRoute(r, routetypes.GlobalRouteSet())
	defaultIpv4RouteRemoved := id
	defaultIpv4RouteRemoved.SetHasDefaultIpv4Route(false)
	if err := verifyWatchResults(interfaces.EventWithChanged(defaultIpv4RouteRemoved)); err != nil {
		t.Fatal(err)
	}

	// DHCP Acquired on the interface.
	blockingWatcher.blockingWatch(t, ch)
	addr := fnet.Ipv4Address{Addr: [4]uint8{192, 168, 0, 4}}
	acquiredAddr := tcpip.AddressWithPrefix{Address: tcpip.AddrFrom4Slice(addr.Addr[:]), PrefixLen: 24}
	leaseLength := dhcp.Seconds(10)
	initUpdatedAt := zxtime.Monotonic(42)
	ifs.dhcpAcquired(context.Background(), tcpip.AddressWithPrefix{}, acquiredAddr, dhcp.Config{UpdatedAt: initUpdatedAt, LeaseLength: leaseLength})
	dhcpAddressAdded := id
	var address interfaces.Address
	address.SetAddr(fnet.Subnet{
		Addr:      fnet.IpAddressWithIpv4(addr),
		PrefixLen: uint8(acquiredAddr.PrefixLen),
	})
	address.SetValidUntil(initUpdatedAt.Add(leaseLength.Duration()).MonotonicNano())
	address.SetPreferredLifetimeInfo(interfaces.PreferredLifetimeInfoWithPreferredUntil(int64(zx.TimensecInfinite)))
	address.SetAssignmentState(interfaces.AddressAssignmentStateAssigned)
	dhcpAddressAdded.SetAddresses([]interfaces.Address{address})
	if err := verifyWatchResults(interfaces.EventWithChanged(dhcpAddressAdded)); err != nil {
		t.Fatal(err)
	}

	// DHCP Acquired with same valid_until does not produce event.
	ifs.dhcpAcquired(context.Background(), acquiredAddr, acquiredAddr, dhcp.Config{UpdatedAt: initUpdatedAt, LeaseLength: leaseLength})
	blockingWatcher.blockingWatch(t, ch)

	// DHCP Acquired with different valid_until.
	updatedAt := zxtime.Monotonic(100)
	ifs.dhcpAcquired(context.Background(), acquiredAddr, acquiredAddr, dhcp.Config{UpdatedAt: updatedAt, LeaseLength: leaseLength})
	dhcpAddressRenewed := id
	address.SetValidUntil(updatedAt.Add(leaseLength.Duration()).MonotonicNano())
	dhcpAddressRenewed.SetAddresses([]interfaces.Address{address})
	if err := verifyWatchResults(interfaces.EventWithChanged(dhcpAddressRenewed)); err != nil {
		t.Fatal(err)
	}

	// DHCP Acquired on empty address signaling end of lease.
	blockingWatcher.blockingWatch(t, ch)
	ifs.dhcpAcquired(context.Background(), acquiredAddr, tcpip.AddressWithPrefix{}, dhcp.Config{})
	dhcpExpired := id
	dhcpExpired.SetAddresses([]interfaces.Address{})
	if err := verifyWatchResults(interfaces.EventWithChanged(dhcpExpired)); err != nil {
		t.Fatal(err)
	}

	// Set interface down.
	blockingWatcher.blockingWatch(t, ch)
	if err := ifs.Down(); err != nil {
		t.Fatalf("failed to set interface down: %s", err)
	}
	offline := id
	offline.SetOnline(false)
	if err := verifyWatchResults(interfaces.EventWithChanged(offline)); err != nil {
		t.Fatal(err)
	}

	// Remove the interface.
	blockingWatcher.blockingWatch(t, ch)
	ifs.RemoveByUser()
	if err := verifyWatchResults(interfaces.EventWithRemoved(uint64(ifs.nicid))); err != nil {
		t.Fatal(err)
	}
}

// TestInterfacesWatcherExistingDeepCopyAddresses ensures that changes to address
// properties do not get retroactively applied to Existing events enqueued in the past.
func TestInterfacesWatcherExistingDeepCopyAddresses(t *testing.T) {
	eventChan, watcherChan := startEventLoop(t)
	si := &interfaceStateImpl{watcherChan: watcherChan}

	initialProperties := testProperties()
	{
		e := interfaceAdded(initialProperties)
		eventChan <- &e
	}

	fakeAddressChanged := func(validUntil tcpip.MonotonicTime) addressChanged {
		return addressChanged{
			nicid:        tcpip.NICID(testId),
			protocolAddr: fidlconv.ToTCPIPProtocolAddress(testIpv4Subnet()),
			lifetimes: stack.AddressLifetimes{
				Deprecated:     false,
				PreferredUntil: tcpip.MonotonicTimeInfinite(),
				ValidUntil:     validUntil,
			},
			state: stack.AddressAssigned,
		}
	}
	{
		e := fakeAddressChanged(tcpip.MonotonicTimeInfinite())
		eventChan <- &e
	}

	// Initialize a watcher so that there is a queued Existing event with the
	// address.
	watcher := initWatcher(t, si, optionsWithFullInterest())
	defer func() {
		if err := watcher.Close(); err != nil {
			t.Errorf("failed to close watcher: %s", err)
		}
	}()

	validUntil := []time.Duration{
		time.Hour,
		time.Hour * 2,
	}
	for _, validUntil := range validUntil {
		e := fakeAddressChanged(tcpip.MonotonicTime{}.Add(validUntil))
		eventChan <- &e
	}

	// Read all the queued events.
	wantProperties := initialProperties
	wantProperties.SetAddresses([]interfaces.Address{testIpv4Address()})
	event, err := watcher.Watch(context.Background())
	if err := assertWatchResult(event, err, interfaces.EventWithExisting(wantProperties)); err != nil {
		t.Fatal(err)
	}

	watcher.expectIdleEvent(t)

	for i, validUntil := range validUntil {
		var wantChange interfaces.Properties
		wantChange.SetId(testId)
		addr := testIpv4Address()
		addr.SetValidUntil(validUntil.Nanoseconds())
		wantChange.SetAddresses([]interfaces.Address{addr})
		event, err = watcher.Watch(context.Background())
		if err := assertWatchResult(event, err, interfaces.EventWithChanged(wantChange)); err != nil {
			t.Fatalf("valid-until change index %d mismatch: %s", i, err)
		}
	}
}

func TestInterfacesWatcherInterest(t *testing.T) {
	for _, tc := range []struct {
		name        string
		disinterest interfaces.AddressPropertiesInterest
	}{
		{
			name:        "validUntil",
			disinterest: interfaces.AddressPropertiesInterestValidUntil,
		},
		{
			name:        "preferredLifetimeInfo",
			disinterest: interfaces.AddressPropertiesInterestPreferredLifetimeInfo,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {

			eventChan, watcherChan := startEventLoop(t)
			ns, _ := newNetstack(t, netstackTestOptions{
				interfaceEventChan: eventChan,
			})
			si := &interfaceStateImpl{watcherChan: watcherChan}

			interestedWatcher := initWatcher(t, si, optionsWithFullInterest())
			disinterestedWatcher := func() watcherHelper {
				var options interfaces.WatcherOptions
				options.SetAddressPropertiesInterest(tc.disinterest.InvertBits())
				return initWatcher(t, si, options)
			}()
			watchers := []*watcherHelper{&interestedWatcher, &disinterestedWatcher}
			defer func() {
				for i, watcher := range watchers {
					if err := watcher.Close(); err != nil {
						t.Errorf("failed to close watcher %d: %s", i, err)
					}
				}
			}()

			ifs := addNoopEndpoint(t, ns, "")
			// Must bring up the interface so that addresses
			// can be observed via the watcher.
			if err := ifs.Up(); err != nil {
				t.Fatalf("ifs.Up() = %s", err)
			}

			for i, watcher := range watchers {
				watcher.expectIdleEvent(t)

				event, err := watcher.Watch(context.Background())
				if err := assertWatchResult(event, err, interfaces.EventWithAdded(initialProperties(ifs, ns.name(ifs.nicid)))); err != nil {
					t.Fatalf("watcher index %d error: %s", i, err)
				}

				{
					var onlineChanged interfaces.Properties
					onlineChanged.SetId(testId)
					onlineChanged.SetOnline(true)
					event, err := watcher.Watch(context.Background())
					if err := assertWatchResult(event, err, interfaces.EventWithChanged(onlineChanged)); err != nil {
						t.Fatalf("watcher index %d error: %s", i, err)
					}
				}
			}

			ifs.addAddress(fidlconv.ToTCPIPProtocolAddress(testIpv4Subnet()), stack.AddressProperties{
				Lifetimes: stack.AddressLifetimes{
					Deprecated:     false,
					PreferredUntil: tcpip.MonotonicTimeInfinite(),
					ValidUntil:     tcpip.MonotonicTimeInfinite(),
				},
			})

			// Interested watcher receives all fields; disinterested watcher does
			// not observe the field it isn't interested in.
			{
				event, err := interestedWatcher.Watch(context.Background())
				var want interfaces.Properties
				want.SetId(testId)
				want.SetAddresses([]interfaces.Address{testIpv4Address()})
				if err := assertWatchResult(event, err, interfaces.EventWithChanged(want)); err != nil {
					t.Fatalf("interested watcher should receive all fields when address is added: %s", err)
				}
			}
			{
				event, err := disinterestedWatcher.Watch(context.Background())
				want := func() interfaces.Properties {
					var want interfaces.Properties
					want.SetId(testId)
					addr := testIpv4Address()
					switch tc.disinterest {
					case interfaces.AddressPropertiesInterestValidUntil:
						addr.SetValidUntil(0)
						addr.ClearValidUntil()
					case interfaces.AddressPropertiesInterestPreferredLifetimeInfo:
						addr.SetPreferredLifetimeInfo(interfaces.PreferredLifetimeInfo{})
						addr.ClearPreferredLifetimeInfo()
					default:
						t.Fatalf("unexpected disinterest: %s", tc.disinterest)
					}
					want.SetAddresses([]interfaces.Address{addr})
					return want
				}()
				if err := assertWatchResult(event, err, interfaces.EventWithChanged(want)); err != nil {
					t.Fatalf("disinterested watcher should not receive %s when address is added: %s", tc.name, err)
				}
			}

			lifetimes := stack.AddressLifetimes{
				Deprecated:     false,
				PreferredUntil: tcpip.MonotonicTimeInfinite(),
				ValidUntil:     tcpip.MonotonicTimeInfinite(),
			}
			const changedLifetime = time.Hour
			switch tc.disinterest {
			case interfaces.AddressPropertiesInterestValidUntil:
				lifetimes.ValidUntil = tcpip.MonotonicTime{}.Add(changedLifetime)
			case interfaces.AddressPropertiesInterestPreferredLifetimeInfo:
				lifetimes.PreferredUntil = tcpip.MonotonicTime{}.Add(changedLifetime)
			default:
				t.Fatalf("unexpected disinterest: %s", tc.disinterest)
			}
			ifs.ns.stack.SetAddressLifetimes(tcpip.NICID(testId), fidlconv.ToTCPIPAddressWithPrefix(testIpv4Subnet()).Address, lifetimes)
			ifs.removeAddress(fidlconv.ToTCPIPProtocolAddress(testIpv4Subnet()))
			// Interested watcher receives the changed event and then the
			// address-removed event; disinterested watcher only receives the
			// address-removed event.
			{
				event, err := interestedWatcher.Watch(context.Background())
				want := func() interfaces.Properties {
					var properties interfaces.Properties
					properties.SetId(testId)
					addr := testIpv4Address()
					switch tc.disinterest {
					case interfaces.AddressPropertiesInterestValidUntil:
						addr.SetValidUntil(changedLifetime.Nanoseconds())
					case interfaces.AddressPropertiesInterestPreferredLifetimeInfo:
						addr.SetPreferredLifetimeInfo(interfaces.PreferredLifetimeInfoWithPreferredUntil(changedLifetime.Nanoseconds()))
					default:
						t.Fatalf("unexpected disinterest: %s", tc.disinterest)
					}
					properties.SetAddresses([]interfaces.Address{addr})
					return properties
				}()
				if err := assertWatchResult(event, err, interfaces.EventWithChanged(want)); err != nil {
					t.Fatalf("interested watcher receives %s change: %s", tc.name, err)
				}
			}
			for i, watcher := range watchers {
				event, err := watcher.Watch(context.Background())
				var want interfaces.Properties
				want.SetId(testId)
				want.SetAddresses(nil)
				if err := assertWatchResult(event, err, interfaces.EventWithChanged(want)); err != nil {
					t.Fatalf("failed to observe address-removed event on watcher index %d: %s", i, err)
				}
			}
		})
	}
}

// TestInterfacesWatcherAddressState tests that the interface watcher event
// loop keeps track of address state correctly by emitting fake state change
// events and ensuring the address appears or disappears as appropriate.
func TestInterfacesWatcherAddressState(t *testing.T) {
	states := []stack.AddressAssignmentState{
		stack.AddressAssigned,
		stack.AddressTentative,
		stack.AddressDisabled,
	}
	for _, fromState := range states {
		for _, toState := range states {
			if fromState != toState {
				t.Run(fmt.Sprintf("%s_to_%s", fromState, toState), func(t *testing.T) {
					protocolAddr := tcpip.ProtocolAddress{
						Protocol: header.IPv6ProtocolNumber,
						AddressWithPrefix: tcpip.AddressWithPrefix{
							Address:   util.Parse("abcd::1"),
							PrefixLen: 64,
						},
					}
					eventChan, watcherChan := startEventLoop(t)
					ns, _ := newNetstack(t, netstackTestOptions{
						interfaceEventChan: eventChan,
					})

					si := &interfaceStateImpl{watcherChan: watcherChan}

					ifs := addNoopEndpoint(t, ns, "")
					// Must bring up the interface as otherwise IPv6 addresses
					// in Tentative or Disabled are not observed.
					if err := ifs.Up(); err != nil {
						t.Fatalf("ifs.Up() = %s", err)
					}

					watcher := initWatcher(t, si, optionsWithFullInterest())
					defer func() {
						if err := watcher.Close(); err != nil {
							t.Fatalf("failed to close watcher: %s", err)
						}
					}()

					event, err := watcher.Watch(context.Background())
					properties := initialProperties(ifs, ns.name(ifs.nicid))
					properties.SetOnline(true)
					if err := assertWatchResult(event, err, interfaces.EventWithExisting(properties)); err != nil {
						t.Fatal(err)
					}
					watcher.expectIdleEvent(t)

					// Add an IPv6 address, since DAD is disabled should
					// immediately observe it as assigned.
					ifs.addAddress(protocolAddr, stack.AddressProperties{})

					var wantAddress interfaces.Address
					wantAddress.SetAddr(fidlconv.ToNetSubnet(protocolAddr.AddressWithPrefix))
					wantAddress.SetValidUntil(int64(zx.TimensecInfinite))
					wantAddress.SetPreferredLifetimeInfo(interfaces.PreferredLifetimeInfoWithPreferredUntil(int64(zx.TimensecInfinite)))
					wantAddress.SetAssignmentState(interfaces.AddressAssignmentStateAssigned)
					var propertiesWithAddress interfaces.Properties
					propertiesWithAddress.SetId(uint64(ifs.nicid))
					propertiesWithAddress.SetAddresses([]interfaces.Address{wantAddress})
					var propertiesWithoutAddress interfaces.Properties
					propertiesWithoutAddress.SetId(uint64(ifs.nicid))
					propertiesWithoutAddress.SetAddresses(nil)
					event, err = watcher.Watch(context.Background())
					if err := assertWatchResult(event, err, interfaces.EventWithChanged(propertiesWithAddress)); err != nil {
						t.Fatal(err)
					}

					var states []stack.AddressAssignmentState
					if fromState != stack.AddressAssigned {
						states = append(states, fromState)
					}
					states = append(states, toState)
					if toState != stack.AddressAssigned {
						states = append(states, stack.AddressAssigned)
					}
					currentState := stack.AddressAssigned
					for _, nextState := range states {
						// Fake an event changing the assignment state.
						ns.interfaceEventChan <- &addressChanged{
							nicid:        ifs.nicid,
							protocolAddr: protocolAddr,
							state:        nextState,
						}

						change, changed := func() (interfaces.Properties, bool) {
							if currentState == stack.AddressAssigned &&
								(nextState == stack.AddressDisabled || nextState == stack.AddressTentative) {
								return propertiesWithoutAddress, true
							} else if (currentState == stack.AddressDisabled || currentState == stack.AddressTentative) &&
								nextState == stack.AddressAssigned {
								return propertiesWithAddress, true
							}
							return interfaces.Properties{}, false
						}()
						if changed {
							event, err := watcher.Watch(context.Background())
							if err := assertWatchResult(event, err, interfaces.EventWithChanged(change)); err != nil {
								t.Fatalf("state %s to %s: %s", currentState, nextState, err)
							}
						}
						currentState = nextState
					}

					// Remove the address and observe removal.
					if status := ifs.removeAddress(protocolAddr); status != zx.ErrOk {
						t.Fatalf("ifs.removeAddress(%#v) = %s", protocolAddr, status)
					}
					{
						event, err := watcher.Watch(context.Background())
						if err := assertWatchResult(event, err, interfaces.EventWithChanged(propertiesWithoutAddress)); err != nil {
							t.Fatal(err)
						}
					}
				})
			}
		}
	}
}

func TestAddressesChangeType(t *testing.T) {
	for _, tc := range []struct {
		name                           string
		previouslyKnown                bool
		prevProperties, nextProperties addressProperties
		wantChanges                    changedAddressProperties
	}{
		{
			name:            "not previously known and visible",
			previouslyKnown: false,
			prevProperties:  addressProperties{},
			nextProperties: addressProperties{
				state: stack.AddressAssigned,
			},
			wantChanges: changedAddressProperties{
				properties:      interfaces.AddressPropertiesInterest(0),
				assignmentState: assignmentStateChangeInvolvingAssigned,
			},
		},
		{
			name:            "not previously known and not visible",
			previouslyKnown: false,
			prevProperties:  addressProperties{},
			nextProperties: addressProperties{
				state: stack.AddressDisabled,
			},
			wantChanges: changedAddressProperties{
				properties:      interfaces.AddressPropertiesInterest(0),
				assignmentState: assignmentStateChangeNotInvolvingAssigned,
			},
		},
		{
			name:            "invisible to visible",
			previouslyKnown: true,
			prevProperties: addressProperties{
				state: stack.AddressDisabled,
			},
			nextProperties: addressProperties{
				state: stack.AddressAssigned,
			},
			wantChanges: changedAddressProperties{
				properties:      interfaces.AddressPropertiesInterest(0),
				assignmentState: assignmentStateChangeInvolvingAssigned,
			},
		},
		{
			name:            "visible to invisible",
			previouslyKnown: true,
			prevProperties: addressProperties{
				state: stack.AddressAssigned,
			},
			nextProperties: addressProperties{
				state: stack.AddressDisabled,
			},
			wantChanges: changedAddressProperties{
				properties:      interfaces.AddressPropertiesInterest(0),
				assignmentState: assignmentStateChangeInvolvingAssigned,
			},
		},
		{
			name:            "invisible property change",
			previouslyKnown: true,
			prevProperties: addressProperties{
				state: stack.AddressDisabled,
				lifetimes: stack.AddressLifetimes{
					Deprecated: true,
				},
			},
			nextProperties: addressProperties{
				state: stack.AddressDisabled,
				lifetimes: stack.AddressLifetimes{
					Deprecated: false,
				},
			},
			wantChanges: changedAddressProperties{
				properties:      interfaces.AddressPropertiesInterestPreferredLifetimeInfo,
				assignmentState: assignmentStateUnchangedAtNonassigned,
			},
		},
		{
			name:            "visible property change",
			previouslyKnown: true,
			prevProperties: addressProperties{
				state: stack.AddressAssigned,
				lifetimes: stack.AddressLifetimes{
					Deprecated: true,
				},
			},
			nextProperties: addressProperties{
				state: stack.AddressAssigned,
				lifetimes: stack.AddressLifetimes{
					Deprecated: false,
				},
			},
			wantChanges: changedAddressProperties{
				properties:      interfaces.AddressPropertiesInterestPreferredLifetimeInfo,
				assignmentState: assignmentStateUnchangedAtAssigned,
			},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if gotChanges := addressesChangeType(
				tc.previouslyKnown,
				tc.prevProperties,
				tc.nextProperties,
			); gotChanges != tc.wantChanges {
				t.Errorf("got addressChangeType(%t, %#v, %#v) = %#v, want = %#v",
					tc.previouslyKnown,
					tc.prevProperties,
					tc.nextProperties,
					gotChanges,
					tc.wantChanges,
				)
			}
		})
	}
}

func TestAddressToString(t *testing.T) {
	subnet := testIpv4Subnet()

	for _, tc := range []struct {
		name string
		fn   func(*interfaces.Address)
		want string
	}{
		{
			name: "no lifetimes",
			fn:   func(a *interfaces.Address) {},
			want: "{Addr:1.2.3.4/16}",
		},
		{
			name: "infinite valid-until and preferred-until",
			fn: func(a *interfaces.Address) {
				a.SetValidUntil(int64(zx.TimensecInfinite))
				a.SetPreferredLifetimeInfo(interfaces.PreferredLifetimeInfoWithPreferredUntil(int64(zx.TimensecInfinite)))
			},
			want: "{Addr:1.2.3.4/16, ValidUntil:boot+2562047h47m16.854775807s, PreferredLifetimeInfo:boot+2562047h47m16.854775807s}",
		},
		{
			name: "finite valid-until and deprecated",
			fn: func(a *interfaces.Address) {
				a.SetValidUntil(int64(60_000_000_000))
				a.SetPreferredLifetimeInfo(interfaces.PreferredLifetimeInfoWithDeprecated(interfaces.Empty{}))
			},
			want: "{Addr:1.2.3.4/16, ValidUntil:boot+1m0s, PreferredLifetimeInfo:deprecated}",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			var a interfaces.Address
			a.SetAddr(subnet)
			tc.fn(&a)

			if got := addressToString(a); got != tc.want {
				t.Fatalf("got \"%s\", want \"%s\"", got, tc.want)
			}
		})
	}
}

func TestInterfaceAddedStringer(t *testing.T) {
	want := "{Id:1, Name:testif01, PortClass:loopback, Online:true, HasDefaultIpv4Route:true, HasDefaultIpv6Route:true}"
	if got := interfaceAdded(testProperties()).String(); got != want {
		t.Fatalf("got \"%s\", want \"%s\"", got, want)
	}
}

func TestDefaultRouteChangedStringer(t *testing.T) {
	ipv4, ipv6 := true, true
	for _, tc := range []struct {
		name  string
		event defaultRouteChanged
		want  string
	}{
		{
			name: "neither present",
			event: defaultRouteChanged{
				nicid: tcpip.NICID(1),
			},
			want: "{nicid:1}",
		},
		{
			name: "both present",
			event: defaultRouteChanged{
				nicid:               tcpip.NICID(1),
				hasDefaultIPv4Route: &ipv4,
				hasDefaultIPv6Route: &ipv6,
			},
			want: "{nicid:1, hasDefaultIPv4Route:true, hasDefaultIPv6Route:true}",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := tc.event.String(); got != tc.want {
				t.Fatalf("got = \"%s\", want = \"%s\"", got, tc.want)
			}
		})
	}
}

func TestAddressChangedStringer(t *testing.T) {
	a := addressChanged{
		nicid: tcpip.NICID(1),
		protocolAddr: tcpip.ProtocolAddress{
			AddressWithPrefix: tcpip.AddressWithPrefix{Address: util.Parse("1.2.3.4"), PrefixLen: 16},
		},
		lifetimes: stack.AddressLifetimes{
			Deprecated:     false,
			PreferredUntil: (tcpip.MonotonicTime{}).Add(time.Minute),
			ValidUntil:     (tcpip.MonotonicTime{}).Add(time.Minute),
		},
		state: stack.AddressAssigned,
	}
	want := "{nicid:1 addr:1.2.3.4/16 lifetimes:{Deprecated:false PreferredUntil:boot+1m0s ValidUntil:boot+1m0s} state:Assigned}"
	if got := a.String(); got != want {
		t.Fatalf("got = \"%s\", want = \"%s\"", got, want)
	}
}

func TestAddressRemovedStringer(t *testing.T) {
	a := addressRemoved{
		nicid: tcpip.NICID(1),
		protocolAddr: tcpip.ProtocolAddress{
			AddressWithPrefix: tcpip.AddressWithPrefix{Address: util.Parse("1.2.3.4"), PrefixLen: 16},
		},
		reason: stack.AddressRemovalManualAction,
	}
	want := "{nicid:1 addr:1.2.3.4/16 reason:ManualAction}"
	if got := a.String(); got != want {
		t.Fatalf("got = \"%s\", want = \"%s\"", got, want)
	}
}

func TestPortClassConversions(t *testing.T) {
	// Verify that `deviceClassFromPortClass` has a conversion for all possible
	// variants of PortClass.
	var arbitraryPortClass network.PortClass
	for _, portClass := range network.PortClass.I_EnumValues(arbitraryPortClass) {
		deviceClassFromPortClass(portClass)
	}
}
