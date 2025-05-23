// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package netstack

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"sort"
	"strings"
	"sync/atomic"
	"syscall/zx"
	"syscall/zx/fidl"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/fidlconv"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/sync"
	zxtime "go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/time"
	"go.fuchsia.dev/fuchsia/src/lib/component"
	syslog "go.fuchsia.dev/fuchsia/src/lib/syslog/go"

	"fidl/fuchsia/hardware/network"
	"fidl/fuchsia/net"
	"fidl/fuchsia/net/interfaces"

	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/stack"
)

const watcherProtocolName = "fuchsia.net.interfaces/Watcher"

func initialProperties(ifs *ifState, name string) interfaces.Properties {
	var p interfaces.Properties

	p.SetId(uint64(ifs.nicid))
	p.SetName(name)
	if ifs.endpoint.Capabilities()&stack.CapabilityLoopback != 0 {
		p.SetPortClass(interfaces.PortClassWithLoopback(interfaces.Empty{}))
		// TODO(https://fxbug.dev/42157740): Remove this field.
		p.SetDeviceClass(interfaces.DeviceClassWithLoopback(interfaces.Empty{}))
	} else if ifs.controller != nil {
		p.SetPortClass(interfaces.PortClassWithDevice(ifs.controller.PortClass()))
		// TODO(https://fxbug.dev/42157740): Remove this field.
		deviceClass := deviceClassFromPortClass(ifs.controller.PortClass())
		p.SetDeviceClass(interfaces.DeviceClassWithDevice(deviceClass))

	} else {
		panic(fmt.Sprintf("can't extract DeviceClass from non-loopback NIC %d(%s) with nil controller", ifs.nicid, name))
	}

	p.SetOnline(false)
	p.SetHasDefaultIpv4Route(false)
	p.SetHasDefaultIpv6Route(false)
	p.SetAddresses([]interfaces.Address{})

	return p
}

// TODO(https://fxbug.dev/42157740): Remove this converter.
// deviceClassFromPortClass provides a backwards compatible conversion between
// the deprecated `DeviceClass` and it's replacement `PortClass`.
func deviceClassFromPortClass(portClass network.PortClass) network.DeviceClass {
	switch portClass {
	case network.PortClassVirtual:
		return network.DeviceClassVirtual
	case network.PortClassBridge:
		return network.DeviceClassBridge
	case network.PortClassPpp:
		return network.DeviceClassPpp
	case network.PortClassEthernet:
		return network.DeviceClassEthernet
	case network.PortClassWlanClient:
		return network.DeviceClassWlan
	case network.PortClassWlanAp:
		return network.DeviceClassWlanAp
	case network.PortClassLowpan:
		// NB: There is no equivalent for lowpan in `DeviceClass`. Represent it
		// as virtual.
		return network.DeviceClassVirtual
	default:
		panic(fmt.Sprintf("can't extract DeviceClass from unknown PortClass (%d)", portClass))
	}
}

func hasAllSecondaryProperties(addr interfaces.Address) bool {
	return addr.HasValidUntil() && addr.HasPreferredLifetimeInfo()
}

var _ interfaces.WatcherWithCtx = (*interfaceWatcherImpl)(nil)

type interfaceWatcherImpl struct {
	cancelServe                 context.CancelFunc
	ready                       chan struct{}
	addrInterest                interfaces.AddressPropertiesInterest
	includeNonAssignedAddresses bool
	mu                          struct {
		sync.Mutex
		isHanging bool
		queue     []interfaces.Event
	}
}

const maxInterfaceWatcherQueueLen = 128

func (wi *interfaceWatcherImpl) onEvent(e interfaces.Event) {
	wi.mu.Lock()
	if len(wi.mu.queue) >= maxInterfaceWatcherQueueLen {
		_ = syslog.ErrorTf(watcherProtocolName, "too many unconsumed events (client may not be calling Watch as frequently as possible): %d, max: %d", len(wi.mu.queue), maxInterfaceWatcherQueueLen)
		wi.cancelServe()
	} else {
		wi.mu.queue = append(wi.mu.queue, e)
	}
	queueLen := len(wi.mu.queue)
	isHanging := wi.mu.isHanging
	wi.mu.Unlock()

	if queueLen > 0 && isHanging {
		select {
		case wi.ready <- struct{}{}:
		default:
		}
	}
}

// filterAddressesAndProperties returns a list of addresses with disinterested
// addresses and properties cleared.
//
// Does not modify addresses and returns a new slice.
func (wi *interfaceWatcherImpl) filterAddressesAndProperties(addresses []interfaces.Address) []interfaces.Address {
	rtn := make([]interfaces.Address, 0, len(addresses))
	clearValidUntil := !wi.addrInterest.HasBits(interfaces.AddressPropertiesInterestValidUntil)
	clearPreferredLifetimeInfo := !wi.addrInterest.HasBits(interfaces.AddressPropertiesInterestPreferredLifetimeInfo)
	for _, addr := range addresses {
		if !wi.includeNonAssignedAddresses {
			switch state := addr.GetAssignmentState(); state {
			case interfaces.AddressAssignmentStateAssigned:
			case interfaces.AddressAssignmentStateTentative, interfaces.AddressAssignmentStateUnavailable:
				// If the address is not assigned and the watcher does not want us to
				// include non-assigned addresses, skip the address.
				continue
			default:
				panic(fmt.Sprintf("unexpected assignment state = %d", state))
			}
		}

		if clearValidUntil {
			addr.ClearValidUntil()
		}
		if clearPreferredLifetimeInfo {
			addr.ClearPreferredLifetimeInfo()
		}
		rtn = append(rtn, addr)
	}
	return rtn
}

type assignmentStateChange int

const (
	_ assignmentStateChange = iota
	assignmentStateChangeInvolvingAssigned
	assignmentStateChangeNotInvolvingAssigned
	assignmentStateUnchangedAtAssigned
	assignmentStateUnchangedAtNonassigned
)

type changedAddressProperties struct {
	// When the address is added/removed, the interested properties field is
	// always zero since the only interesting event during such events in the
	// assignment state change.
	//
	// Note that we repurpose interfaces.AddressPropertiesInterest to hold the
	// fields that has changed instead of any specific watcher's interest in
	// address properties. This is so that we can easily check if the set of
	// changed properties intersects with a watcher's set of interested
	// properties.
	properties      interfaces.AddressPropertiesInterest
	assignmentState assignmentStateChange
}

func (c *changedAddressProperties) anyOfInterest(includeAllAddresses bool, includeProperties interfaces.AddressPropertiesInterest) bool {
	switch c.assignmentState {
	case assignmentStateChangeInvolvingAssigned:
		// All watchers receive updates if the state transitioned from/to assigned.
		return true
	case assignmentStateChangeNotInvolvingAssigned:
		// This is change where the previous and new state are both not assigned.
		// This change is only of interest to the watcher if they are interested
		// in all addresses. Note that the other address properties don't matter
		// here since if the address is not of interest to watchers, then property
		// changes aren't either for such addresses.
		return includeAllAddresses
	case assignmentStateUnchangedAtNonassigned:
		// If the addresses remained at an unassigned state and the watcher is not
		// interested in those addresses, then the properties are not of interest
		// either.
		if !includeAllAddresses {
			return false
		}
		fallthrough
	case assignmentStateUnchangedAtAssigned:
		// All watchers are interested in changes to assigned addresses so if the
		// properties changed, then it is of interest to the watcher.
		return c.properties&includeProperties != 0
	default:
		panic(fmt.Sprintf("unexpected assignment state change = %d", c.assignmentState))
	}
}

// onAddressesChanged handles an address for this watcher client.
//
// Does not modify addresses.
func (wi *interfaceWatcherImpl) onAddressesChanged(nicid tcpip.NICID, addresses []interfaces.Address, changes changedAddressProperties) {
	if !changes.anyOfInterest(wi.includeNonAssignedAddresses, wi.addrInterest) {
		// No changes of interest happened to the address.
		return
	}
	var changed interfaces.Properties
	changed.SetId(uint64(nicid))
	changed.SetAddresses(wi.filterAddressesAndProperties(addresses))
	wi.onEvent(interfaces.EventWithChanged(changed))
}

func cmpSubnet(ifAddr1 net.Subnet, ifAddr2 net.Subnet) int {
	switch ifAddr1.Addr.Which() {
	case net.IpAddressIpv4:
		if ifAddr2.Addr.Which() == net.IpAddressIpv6 {
			return -1
		}
		if diff := bytes.Compare(ifAddr1.Addr.Ipv4.Addr[:], ifAddr2.Addr.Ipv4.Addr[:]); diff != 0 {
			return diff
		}
	case net.IpAddressIpv6:
		if ifAddr2.Addr.Which() == net.IpAddressIpv4 {
			return 1
		}
		if diff := bytes.Compare(ifAddr1.Addr.Ipv6.Addr[:], ifAddr2.Addr.Ipv6.Addr[:]); diff != 0 {
			return diff
		}
	}
	if ifAddr1.PrefixLen < ifAddr2.PrefixLen {
		return -1
	} else if ifAddr1.PrefixLen > ifAddr2.PrefixLen {
		return 1
	}
	return 0
}

func (wi *interfaceWatcherImpl) Watch(ctx fidl.Context) (interfaces.Event, error) {
	wi.mu.Lock()
	defer wi.mu.Unlock()

	if wi.mu.isHanging {
		wi.cancelServe()
		return interfaces.Event{}, errors.New("not allowed to call Watcher.Watch when a call is already pending")
	}

	for {
		if len(wi.mu.queue) > 0 {
			event := wi.mu.queue[0]
			wi.mu.queue[0] = interfaces.Event{}
			wi.mu.queue = wi.mu.queue[1:]
			if len(wi.mu.queue) == 0 {
				// Drop the whole slice so that the backing array can be garbage
				// collected. Otherwise, the now-inaccessible front of wi.mu.queue could
				// be retained in memory forever.
				wi.mu.queue = nil
			}
			return event, nil
		}

		wi.mu.isHanging = true
		wi.mu.Unlock()

		var err error
		select {
		case <-wi.ready:
		case <-ctx.Done():
			err = fmt.Errorf("cancelled: %w", ctx.Err())
		}

		wi.mu.Lock()
		wi.mu.isHanging = false
		if err != nil {
			return interfaces.Event{}, err
		}
	}
}

type interfaceWatcherRequest struct {
	req     interfaces.WatcherWithCtxInterfaceRequest
	options interfaces.WatcherOptions
}

var _ interfaces.StateWithCtx = (*interfaceStateImpl)(nil)

type interfaceStateImpl struct {
	watcherChan chan<- interfaceWatcherRequest
}

func (si *interfaceStateImpl) GetWatcher(_ fidl.Context, options interfaces.WatcherOptions, watcher interfaces.WatcherWithCtxInterfaceRequest) error {
	si.watcherChan <- interfaceWatcherRequest{
		req:     watcher,
		options: options,
	}
	return nil
}

func fidlSubnetToString(s net.Subnet) string {
	return fidlconv.ToTCPIPAddressWithPrefix(s).String()
}

func preferredLifetimeInfoToString(p interfaces.PreferredLifetimeInfo) string {
	switch tag := p.Which(); tag {
	case interfaces.PreferredLifetimeInfoDeprecated:
		return "deprecated"
	case interfaces.PreferredLifetimeInfoPreferredUntil:
		return fmt.Sprintf("boot+%s", zxtime.Duration(p.PreferredUntil))
	default:
		panic(fmt.Sprintf("fuchsia.net.interfaces/PreferredLifetimeInfo with unknown tag: %d", tag))
	}
}

func addressToString(a interfaces.Address) string {
	var b strings.Builder

	if !a.HasAddr() {
		// Note that due to the lack of addr, this will not accidentally print an
		// address in bytes that prevents anonymization.
		panic(fmt.Sprintf("fuchsia.net.interfaces/Address must contain addr field: %#v", a))
	}
	b.WriteString(fmt.Sprintf("{Addr:%s", fidlSubnetToString(a.GetAddr())))
	if a.HasValidUntil() {
		b.WriteString(fmt.Sprintf(", ValidUntil:boot+%s", zxtime.Duration(a.GetValidUntil())))
	}
	if a.HasPreferredLifetimeInfo() {
		b.WriteString(fmt.Sprintf(", PreferredLifetimeInfo:%s", preferredLifetimeInfoToString(a.GetPreferredLifetimeInfo())))
	}
	b.WriteString("}")

	return b.String()
}

type interfaceEvent interface {
	isInterfaceEvent()
}

type interfaceAdded interfaces.Properties

var _ interfaceEvent = (*interfaceAdded)(nil)

func (*interfaceAdded) isInterfaceEvent() {}

func portClassToString(d interfaces.PortClass) string {
	switch tag := d.Which(); tag {
	case interfaces.PortClassLoopback:
		return "loopback"
	case interfaces.PortClassDevice:
		return d.Device.String()
	default:
		panic(fmt.Sprintf("fuchsia.net.interfaces/PortClass with unknown tag: %d", tag))
	}
}

func (a interfaceAdded) String() string {
	p := interfaces.Properties(a)
	var b strings.Builder

	if !p.HasId() {
		panic("fuchsia.net.interfaces/Properties missing id field")
	}

	b.WriteString(fmt.Sprintf("{Id:%d", p.GetId()))
	if p.HasName() {
		b.WriteString(fmt.Sprintf(", Name:%s", p.GetName()))
	}
	if p.HasPortClass() {
		b.WriteString(fmt.Sprintf(", PortClass:%s", portClassToString(p.GetPortClass())))
	}
	if p.HasOnline() {
		b.WriteString(fmt.Sprintf(", Online:%t", p.GetOnline()))
	}
	if p.HasHasDefaultIpv4Route() {
		b.WriteString(fmt.Sprintf(", HasDefaultIpv4Route:%t", p.GetHasDefaultIpv4Route()))
	}
	if p.HasHasDefaultIpv6Route() {
		b.WriteString(fmt.Sprintf(", HasDefaultIpv6Route:%t", p.GetHasDefaultIpv6Route()))
	}
	if p.HasAddresses() && len(p.GetAddresses()) > 0 {
		b.WriteString(", Addresses:[")
		for i, addr := range p.GetAddresses() {
			if i > 0 {
				b.WriteString(", ")
			}
			b.WriteString(addressToString(addr))
		}
		b.WriteString("]")
	}
	b.WriteString("}")

	return b.String()
}

type interfaceRemoved tcpip.NICID

var _ interfaceEvent = (*interfaceRemoved)(nil)

func (*interfaceRemoved) isInterfaceEvent() {}

type onlineChanged struct {
	nicid  tcpip.NICID
	online bool
}

var _ interfaceEvent = (*onlineChanged)(nil)

func (*onlineChanged) isInterfaceEvent() {}

type defaultRouteChanged struct {
	nicid               tcpip.NICID
	hasDefaultIPv4Route *bool
	hasDefaultIPv6Route *bool
}

var _ interfaceEvent = (*defaultRouteChanged)(nil)

func (*defaultRouteChanged) isInterfaceEvent() {}

func (c defaultRouteChanged) String() string {
	var b strings.Builder

	b.WriteString(fmt.Sprintf("{nicid:%d", c.nicid))
	if c.hasDefaultIPv4Route != nil {
		b.WriteString(fmt.Sprintf(", hasDefaultIPv4Route:%t", *c.hasDefaultIPv4Route))
	}
	if c.hasDefaultIPv6Route != nil {
		b.WriteString(fmt.Sprintf(", hasDefaultIPv6Route:%t", *c.hasDefaultIPv6Route))
	}
	b.WriteString("}")

	return b.String()
}

type addressProperties struct {
	lifetimes stack.AddressLifetimes
	state     stack.AddressAssignmentState
}

// isAddressAssigned returns whether an address is considered assigned.
func isAddressAssigned(state stack.AddressAssignmentState) bool {
	switch state {
	case stack.AddressAssigned:
		return true
	case stack.AddressDisabled, stack.AddressTentative:
		return false
	default:
		panic(fmt.Sprintf("unknown address assignment state: %d", state))
	}
}

type interfaceProperties struct {
	interfaces.Properties
	// addresses stores address properties that come from the gVisor stack.
	//
	// It is necessary to track these properties separately because IPv6
	// addresses in disabled or tentative state are hidden from clients
	// so such addresses are not present in the embedded Properties and
	// need to have their properties stored here.
	addresses map[tcpip.ProtocolAddress]addressProperties
}

func addressMapToSlice(addressMap map[tcpip.ProtocolAddress]addressProperties) []interfaces.Address {
	var addressSlice []interfaces.Address
	for protocolAddr, properties := range addressMap {
		var addr interfaces.Address
		addr.SetAddr(fidlconv.ToNetSubnet(protocolAddr.AddressWithPrefix))
		addr.SetValidUntil(int64(toZxTimeInfiniteIfZero(properties.lifetimes.ValidUntil)))
		info := func() interfaces.PreferredLifetimeInfo {
			if properties.lifetimes.Deprecated {
				return interfaces.PreferredLifetimeInfoWithDeprecated(interfaces.Empty{})
			} else {
				return interfaces.PreferredLifetimeInfoWithPreferredUntil(int64(toZxTimeInfiniteIfZero(properties.lifetimes.PreferredUntil)))
			}
		}()
		addr.SetPreferredLifetimeInfo(info)
		addr.SetAssignmentState(fidlconv.ToAddressAssignmentState(properties.state))
		addressSlice = append(addressSlice, addr)
	}

	sort.Slice(addressSlice, func(i, j int) bool {
		return cmpSubnet(addressSlice[i].GetAddr(), addressSlice[j].GetAddr()) <= 0
	})
	return addressSlice
}

func toZxTimeInfiniteIfZero(t tcpip.MonotonicTime) zx.Time {
	if t == (tcpip.MonotonicTime{}) {
		return zx.TimensecInfinite
	}
	return fidlconv.ToZxTime(t)
}

type fidlInterfaceWatcherStats struct {
	count atomic.Int64
}

func addedRemovedAddressChangeType(properties addressProperties) changedAddressProperties {
	var assignmentState assignmentStateChange
	if isAddressAssigned(properties.state) {
		assignmentState = assignmentStateChangeInvolvingAssigned
	} else {
		assignmentState = assignmentStateChangeNotInvolvingAssigned
	}
	return changedAddressProperties{assignmentState: assignmentState}
}

func addressesChangeType(previouslyKnown bool, prevProperties, nextProperties addressProperties) changedAddressProperties {
	if !previouslyKnown {
		// If the address was previously unknown, treat the "previous" assignment
		// state like it was some imaginary state indicating "not added".
		return addedRemovedAddressChangeType(nextProperties)
	}

	var properties interfaces.AddressPropertiesInterest
	if prevProperties.lifetimes.ValidUntil != nextProperties.lifetimes.ValidUntil {
		properties |= interfaces.AddressPropertiesInterestValidUntil
	}
	if prevProperties.lifetimes.Deprecated != nextProperties.lifetimes.Deprecated ||
		prevProperties.lifetimes.PreferredUntil != nextProperties.lifetimes.PreferredUntil {
		properties |= interfaces.AddressPropertiesInterestPreferredLifetimeInfo
	}

	assignmentState := func() assignmentStateChange {
		if prevProperties.state == nextProperties.state {
			if isAddressAssigned(nextProperties.state) {
				return assignmentStateUnchangedAtAssigned
			}
			return assignmentStateUnchangedAtNonassigned
		}

		if isAddressAssigned(prevProperties.state) || isAddressAssigned(nextProperties.state) {
			return assignmentStateChangeInvolvingAssigned
		}

		return assignmentStateChangeNotInvolvingAssigned
	}()

	return changedAddressProperties{
		properties:      properties,
		assignmentState: assignmentState,
	}
}

func interfaceWatcherEventLoop(
	ctx context.Context,
	eventChan <-chan interfaceEvent,
	watcherChan <-chan interfaceWatcherRequest,
	fidlInterfaceWatcherStats *fidlInterfaceWatcherStats,
) {
	if eventChan == nil {
		panic("cannot start interface watcher event loop with nil interface event channel")
	}
	if watcherChan == nil {
		panic("cannot start interface watcher event loop with nil watcher channel")
	}

	watchers := make(map[*interfaceWatcherImpl]struct{})
	propertiesMap := make(map[tcpip.NICID]interfaceProperties)
	watcherClosedChan := make(chan *interfaceWatcherImpl)
	watcherClosedFn := func(closedWatcher *interfaceWatcherImpl) {
		delete(watchers, closedWatcher)
		fidlInterfaceWatcherStats.count.Add(-1)
	}

	for {
		select {
		case <-ctx.Done():
			_ = syslog.InfoTf(watcherProtocolName, "stopping interface watcher event loop")

			// Wait for all watchers to close so that it is guaranteed that no
			// goroutines serving the interface watcher API are still running once
			// this function returns.
			for len(watchers) > 0 {
				watcherClosedFn(<-watcherClosedChan)
			}
			return
		case e := <-eventChan:
			switch event := e.(type) {
			case *interfaceAdded:
				added := interfaces.Properties(*event)
				if !added.HasId() {
					panic(fmt.Sprintf("interface added event with no ID: %s", event))
				}
				if len(added.GetAddresses()) > 0 {
					// This panic enforces that interfaces are never added
					// with addresses present, which enables the event loop to
					// not have to worry about address properties/assignment
					// state when handling interface-added events.
					panic(fmt.Sprintf("interface added event contains addresses: %s", event))
				}
				nicid := tcpip.NICID(added.GetId())
				if _, ok := propertiesMap[nicid]; ok {
					panic(fmt.Sprintf("interface already exists but duplicate added event received: %s", event))
				}
				propertiesMap[nicid] = interfaceProperties{
					Properties: added,
					addresses:  make(map[tcpip.ProtocolAddress]addressProperties),
				}
				for w := range watchers {
					properties := added
					// Since added interfaces must not have any addresses, explicitly set
					// the addresses field to nil instead of potentially copying a slice
					// of length 0.
					properties.SetAddresses(nil)
					w.onEvent(interfaces.EventWithAdded(properties))
				}
			case *interfaceRemoved:
				removed := tcpip.NICID(*event)
				syslog.InfoTf(watcherProtocolName, "interface removed event: %d", removed)
				if _, ok := propertiesMap[removed]; !ok {
					panic(fmt.Sprintf("unknown interface NIC=%d removed", removed))
					continue
				}
				delete(propertiesMap, removed)
				for w := range watchers {
					w.onEvent(interfaces.EventWithRemoved(uint64(removed)))
				}
			case *defaultRouteChanged:
				syslog.InfoTf(watcherProtocolName, "default route changed event: %s", event)

				properties, ok := propertiesMap[event.nicid]
				// TODO(https://fxbug.dev/42177477): Change to panic once interface properties
				// are guaranteed to not change after an interface is removed.
				if !ok {
					_ = syslog.WarnTf(watcherProtocolName, "default route changed event for unknown interface: %#v", event)
					break
				}
				// TODO(https://fxbug.dev/42177595): Once these events are only emitted when
				// the presence of a default route has actually changed, panic if the event
				// disagrees with our view of the world.
				var changes interfaces.Properties
				if event.hasDefaultIPv4Route != nil && properties.GetHasDefaultIpv4Route() != *event.hasDefaultIPv4Route {
					properties.SetHasDefaultIpv4Route(*event.hasDefaultIPv4Route)
					changes.SetHasDefaultIpv4Route(*event.hasDefaultIPv4Route)
				}
				if event.hasDefaultIPv6Route != nil && properties.GetHasDefaultIpv6Route() != *event.hasDefaultIPv6Route {
					properties.SetHasDefaultIpv6Route(*event.hasDefaultIPv6Route)
					changes.SetHasDefaultIpv6Route(*event.hasDefaultIPv6Route)
				}
				if changes.HasHasDefaultIpv4Route() || changes.HasHasDefaultIpv6Route() {
					propertiesMap[event.nicid] = properties
					changes.SetId(uint64(event.nicid))

					for w := range watchers {
						w.onEvent(interfaces.EventWithChanged(changes))
					}
				}
			case *onlineChanged:
				syslog.InfoTf(watcherProtocolName, "online changed event: %#v", event)

				properties, ok := propertiesMap[event.nicid]
				// TODO(https://fxbug.dev/42177477): Change to panic once interface properties
				// are guaranteed to not change after an interface is removed.
				if !ok {
					_ = syslog.WarnTf(watcherProtocolName, "online changed event for unknown interface: %#v", event)
					break
				}
				if event.online == properties.GetOnline() {
					// This assertion is possible because the event is always emitted under a
					// lock (so cannot race against itself), and the event is only emitted when
					// there is an actual change to the boolean value.
					panic(fmt.Sprintf("online changed event for interface with properties %#v with no actual change", properties))
				}

				properties.SetOnline(event.online)
				propertiesMap[event.nicid] = properties

				var changes interfaces.Properties
				changes.SetId(uint64(event.nicid))
				changes.SetOnline(event.online)
				for w := range watchers {
					w.onEvent(interfaces.EventWithChanged(changes))
				}
			case *addressChanged:
				properties, ok := propertiesMap[event.nicid]
				if !ok {
					panic(fmt.Sprintf("address changed event for unknown interface: %s", event))
				}
				nextProperties := addressProperties{state: event.state, lifetimes: event.lifetimes}
				prevProperties, found := properties.addresses[event.protocolAddr]
				properties.addresses[event.protocolAddr] = nextProperties
				addresses := addressMapToSlice(properties.addresses)
				properties.SetAddresses(addresses)
				propertiesMap[event.nicid] = properties

				// Due to the frequency of lifetime changes in certain environments, don't
				// log when the only difference is in address lifetimes in assigned state.
				if !found || nextProperties.state != prevProperties.state {
					syslog.InfoTf(watcherProtocolName, "address changed event: %s", event)
				}

				changes := addressesChangeType(found, prevProperties, nextProperties)
				if !changes.anyOfInterest(true, interfaces.AddressPropertiesInterest_Mask) {
					break
				}
				for w := range watchers {
					w.onAddressesChanged(event.nicid, addresses, changes)
				}
			case *addressRemoved:
				syslog.InfoTf(watcherProtocolName, "address removed event: %s", event)

				properties, ok := propertiesMap[event.nicid]
				if !ok {
					panic(fmt.Sprintf("address removed event for unknown interface: %s", event))
				}
				addrProperties, ok := properties.addresses[event.protocolAddr]
				if !ok {
					panic(fmt.Sprintf("address removed event for unknown address: %s", event))
				}
				delete(properties.addresses, event.protocolAddr)
				addresses := addressMapToSlice(properties.addresses)
				properties.SetAddresses(addresses)
				propertiesMap[event.nicid] = properties

				// Treat the new assignment state as if it was some imaginary state
				// indicating "not added".
				changes := addedRemovedAddressChangeType(addrProperties)
				for w := range watchers {
					w.onAddressesChanged(event.nicid, addresses, changes)
				}
			}
		case watcher := <-watcherChan:
			watcherCtx, cancel := context.WithCancel(ctx)
			impl := interfaceWatcherImpl{
				ready:                       make(chan struct{}, 1),
				cancelServe:                 cancel,
				addrInterest:                watcher.options.GetAddressPropertiesInterestWithDefault(0),
				includeNonAssignedAddresses: watcher.options.GetIncludeNonAssignedAddressesWithDefault(false),
			}
			impl.mu.queue = make([]interfaces.Event, 0, maxInterfaceWatcherQueueLen)

			for _, properties := range propertiesMap {
				// Filtering address properties returns a deep copy of the
				// addresses so that updates to the current interface state
				// don't accidentally change enqueued events.
				properties := properties.Properties
				properties.SetAddresses(impl.filterAddressesAndProperties(properties.GetAddresses()))
				impl.onEvent(interfaces.EventWithExisting(properties))
			}
			impl.mu.queue = append(impl.mu.queue, interfaces.EventWithIdle(interfaces.Empty{}))

			watchers[&impl] = struct{}{}
			fidlInterfaceWatcherStats.count.Add(1)

			go func() {
				defer cancel()
				component.Serve(watcherCtx, &interfaces.WatcherWithCtxStub{Impl: &impl}, watcher.req.Channel, component.ServeOptions{
					Concurrent: true,
					OnError: func(err error) {
						_ = syslog.WarnTf(watcherProtocolName, "%s", err)
					},
				})

				watcherClosedChan <- &impl
			}()
		case watcherClosed := <-watcherClosedChan:
			watcherClosedFn(watcherClosed)
		}
	}
}
