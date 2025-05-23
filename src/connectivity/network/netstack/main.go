// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package netstack

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"reflect"
	"runtime"
	"strconv"
	"syscall/zx"
	"syscall/zx/fidl"
	"syscall/zx/zxwait"
	"time"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/dns"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/fidlconv"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/filter"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/pprof"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/routes"
	zxtime "go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/time"
	tracingprovider "go.fuchsia.dev/fuchsia/src/connectivity/network/netstack/tracing/provider"

	"go.fuchsia.dev/fuchsia/src/lib/component"
	syslog "go.fuchsia.dev/fuchsia/src/lib/syslog/go"

	"fidl/fuchsia/logger"
	"fidl/fuchsia/net/debug"
	"fidl/fuchsia/net/interfaces"
	"fidl/fuchsia/net/interfaces/admin"
	"fidl/fuchsia/net/name"
	fnetNdp "fidl/fuchsia/net/ndp"
	"fidl/fuchsia/net/neighbor"
	"fidl/fuchsia/net/root"
	fnetRoutes "fidl/fuchsia/net/routes"
	routesAdmin "fidl/fuchsia/net/routes/admin"
	"fidl/fuchsia/net/stack"
	"fidl/fuchsia/posix/socket"
	packetsocket "fidl/fuchsia/posix/socket/packet"
	rawsocket "fidl/fuchsia/posix/socket/raw"
	scheduler "fidl/fuchsia/scheduler/deprecated"
	"fidl/fuchsia/stash"
	"fidl/fuchsia/update/verify"

	"gvisor.dev/gvisor/pkg/atomicbitops"
	glog "gvisor.dev/gvisor/pkg/log"
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/link/sniffer"
	"gvisor.dev/gvisor/pkg/tcpip/network/arp"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv4"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv6"
	tcpipstack "gvisor.dev/gvisor/pkg/tcpip/stack"
	"gvisor.dev/gvisor/pkg/tcpip/transport/icmp"
	"gvisor.dev/gvisor/pkg/tcpip/transport/raw"
	"gvisor.dev/gvisor/pkg/tcpip/transport/tcp"
	"gvisor.dev/gvisor/pkg/tcpip/transport/udp"
)

const (
	// stashStoreIdentificationName is the name used to identify this (netstack)
	// component to the fuchsia.stash.SecureStore service.
	stashStoreIdentificationName = "netstack-stash"

	// opaqueIIDSecretKeyName is the name of the key used to access the secret key
	// for opaque IIDs from the secure stash store.
	opaqueIIDSecretKeyName = "opaque-iid-secret-key"

	// dadTransmits is the number of consecutive NDP Neighbor Solicitation
	// messages sent while performing Duplicate Address Detection on a IPv6
	// tentative address.
	//
	// As per RFC 4862 section 5.1, 1 is the default number of messages to send.
	dadTransmits = 1

	// dadRetransmitTimer is the time between retransmissions of NDP Neighbor
	// Solicitation messages to a neighbor.
	//
	// As per RFC 4861 section 10, 1s is the default time between retransmissions.
	dadRetransmitTimer = time.Second

	// maxRtrSolicitations is the maximum number of Router Solicitation messages
	// to send when a NIC becomes enabled.
	//
	// As per RFC 4861 section 10, 3 is the default number of messages.
	maxRtrSolicitations = 3

	// rtrSolicitationInterval is the amount of time between sending Router
	// Solicitation messages.
	//
	// As per RFC 4861 section 10, 4s is the default time between transmissions.
	rtrSolicitationInterval = 4 * time.Second

	// maxRtrSolicitationDelay is the maximum amount of time to wait before
	// sending the first Router Solicitation message.
	//
	// As per RFC 4861 section 10, 1s is the default maximum time to wait.
	maxRtrSolicitationDelay = time.Second

	// autoGenAddressConflictRetries is the maximum number of times to attempt
	// SLAAC address regeneration in response to DAD conflicts.
	//
	// As per RFC 7217 section, 3 is the default maximum number of retries.
	autoGenAddressConflictRetries = 3

	// maxTempAddrValidLifetime is the maximum amount of time a temporary SLAAC
	// address may be valid for from creation.
	//
	// As per RFC 4941 section 5, 7 days is the default max valid lifetime.
	maxTempAddrValidLifetime = 7 * 24 * time.Hour

	// maxTempAddrPreferredLifetime is the maximum amount of time a temporary
	// SLAAC address may be preferred for from creation.
	//
	// As per RFC 4941 section 5, 1 day is the default max preferred lifetime.
	maxTempAddrPreferredLifetime = 24 * time.Hour

	// regenAdvanceDuration is duration before the deprecation of a temporary
	// address when a new address will be generated.
	//
	// As per RFC 4941 section 5, 5s is the default duration. We make the duration
	// the default duration plus the maximum amount of time for an address to
	// resolve DAD if all but the last regeneration attempts fail. This is to
	// guarantee that if a new address is generated, it will be assigned for at
	// least 5s before the original address is deprecated.
	regenAdvanceDuration = 5*time.Second + dadTransmits*dadRetransmitTimer*(1+autoGenAddressConflictRetries)

	// handleRAs is the configuration for when Router Advertisements should be
	// handled.
	//
	// We want to handle router advertisements even when operating as a router
	// so that we can perform router/prefix discovery and SLAAC.
	handleRAs = ipv6.HandlingRAsAlwaysEnabled
)

type atomicBool struct {
	v *atomicbitops.Uint32
}

// IsBoolFlag implements flag.boolFlag.IsBoolFlag.
//
// See the flag.Value documentation for more information.
func (*atomicBool) IsBoolFlag() bool {
	return true
}

// Set implements flag.Value.Set.
func (a *atomicBool) Set(s string) error {
	v, err := strconv.ParseBool(s)
	if err != nil {
		return err
	}
	var val uint32
	if v {
		val = 1
	}
	a.v.Store(val)
	return nil
}

// String implements flag.Value.String.
func (a *atomicBool) String() string {
	return strconv.FormatBool(a.v.Load() != 0)
}

func init() {
	// As of this writing the default is 1.
	sniffer.LogPackets.Store(0)
}

type glogEmitter struct{}

func (*glogEmitter) Emit(depth int, level glog.Level, timestamp time.Time, format string, v ...interface{}) {
	switch level {
	case glog.Warning:
		syslog.Warnf(format, v...)
	case glog.Info:
		syslog.Infof(format, v...)
	case glog.Debug:
		syslog.Debugf(format, v...)
	}
}

func InstallThreadProfiles(ctx context.Context, componentCtx *component.Context) {
	req, provider, err := scheduler.NewProfileProviderWithCtxInterfaceRequest()
	if err != nil {
		panic(fmt.Sprintf("failed to create %s request: %s", req.Name(), err))
	}
	componentCtx.ConnectToEnvService(req)

	go func() {
		const handlesPerRead = 1
		const bytesPerRead = 1

		channel := zx.GetThreadsChannel()
		for {
			defer func() {
				_ = provider.Close()
			}()
			var handles [handlesPerRead]zx.HandleInfo
			var bytes [bytesPerRead]byte
			var nb, nh uint32
			if err := zxwait.WithRetryContext(ctx, func() error {
				var err error
				nb, nh, err = channel.ReadEtc(bytes[:], handles[:], 0)
				return err
			}, *channel.Handle(), zx.SignalChannelReadable, zx.SignalChannelPeerClosed); err != nil {
				_ = syslog.Errorf("stopped observing for thread profiles: %s", err)
				return
			}
			if nb != bytesPerRead {
				panic(fmt.Sprintf("unexpected %d bytes in channel message", nb))
			}
			if nh != handlesPerRead {
				panic(fmt.Sprintf("unexpected %d handles in channel message", nh))
			}

			handleInfo := handles[0]
			threadType := runtime.ThreadType(bytes[0])

			threadProfile := func() string {
				switch threadType {
				case runtime.WorkerThread:
					return "fuchsia.netstack.go-worker"
				case runtime.SysmonThread:
					return "fuchsia.netstack.go-sysmon"
				default:
					return ""
				}
			}()
			// Skip if no profile is chosen.
			if len(threadProfile) == 0 {
				_ = syslog.Infof("no thread profile for thread type %d", threadType)
				_ = handleInfo.Handle.Close()
				continue
			}
			// Retrieve the koid before transferring the handle.
			koid := func() string {
				info, err := handleInfo.Handle.GetInfoHandleBasic()
				if err != nil {
					return fmt.Sprintf("<%s>", err)
				}
				return fmt.Sprintf("%d", info.Koid)
			}()

			// Attempt to install our thread profile.
			status, err := provider.SetProfileByRole(ctx, handleInfo.Handle, threadProfile)
			if err, ok := err.(*zx.Error); ok {
				switch err.Status {
				case zx.ErrNotFound, zx.ErrPeerClosed, zx.ErrUnavailable:
					_ = syslog.Warnf("connection to %s closed; will not set thread profile %s; FIDL error: %s", req.Name(), threadProfile, err)
					return
				}
			}
			if err != nil {
				_ = syslog.Errorf("failed to set thread profile %s for koid=%s; FIDL error: %s", threadProfile, koid, err)
				continue
			}

			if status := zx.Status(status); status != zx.ErrOk {
				switch status {
				case zx.ErrNotFound:
					_ = syslog.Warnf("did not find thread profile %s for koid=%s", threadProfile, koid)
				default:
					_ = syslog.Errorf("failed to set thread profile %s for koid=%s; rejected with %s", threadProfile, koid, status)
				}
				continue
			}

			_ = syslog.Debugf("successfully set thread profile for koid=%s to %s", koid, threadProfile)
		}
	}()
}

type config struct {
	GOMAXPROCS *int
}

func Main() {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	flags := flag.NewFlagSet(os.Args[0], flag.ContinueOnError)

	logPackets := atomicBool{v: &sniffer.LogPackets}
	flags.Var(&logPackets, "log-packets", "enable packet logging")

	logLevel := syslog.InfoLevel
	flags.Var(&logLevel, "verbosity", "set the logging verbosity")

	var socketStatsTimerPeriod time.Duration
	flags.DurationVar(&socketStatsTimerPeriod, "socket-stats-sampling-interval", time.Minute, "set the interval at which socket stats will be sampled")

	noOpaqueIID := false
	flags.BoolVar(&noOpaqueIID, "no-opaque-iids", false, "disable opaque IIDs")

	fastUDP := true
	flags.BoolVar(&fastUDP, "fast-udp", true, "enable Fast UDP")

	configFile := flags.String("config-data", "/config/data/default.json", "config file to use")

	if err := flags.Parse(os.Args[1:]); err != nil {
		panic(err)
	}

	var config *config
	setMaxProcs := func() bool {
		b, err := os.ReadFile(*configFile)
		if err != nil {
			_ = syslog.Warnf("failed to read config data at %s, continuing with defaults", *configFile)
			return false
		}
		if err := json.Unmarshal(b, &config); err != nil {
			panic(fmt.Sprintf("failed to unmarshal JSON from config file: %s", err))
		}
		if config.GOMAXPROCS != nil {
			prev := runtime.GOMAXPROCS(*config.GOMAXPROCS)
			_ = syslog.Infof("set GOMAXPROCS to %d, was %d (NumCPU = %d)", *config.GOMAXPROCS, prev, runtime.NumCPU())
			return true
		}
		return false
	}()
	if !setMaxProcs {
		_ = syslog.Infof("override for GOMAXPROCS not provided, leaving as default (NumCPU = %d)", runtime.NumCPU())
	}

	componentCtx := component.NewContextFromStartupInfo()

	{
		req, logSink, err := logger.NewLogSinkWithCtxInterfaceRequest()
		if err != nil {
			panic(fmt.Sprintf("failed to create syslog request: %s", err))
		}
		componentCtx.ConnectToEnvService(req)

		options := syslog.LogInitOptions{
			LogLevel: logLevel,
		}
		options.LogSink = logSink
		options.MinSeverityForFileAndLineInfo = logLevel
		options.Tags = []string{"netstack"}

		l, err := syslog.NewLogger(options)
		if err != nil {
			panic(err)
		}
		syslog.SetDefaultLogger(l)
		log.SetOutput(&syslog.Writer{Logger: l})
	}

	log.SetFlags(log.Lshortfile)
	glog.SetTarget(&glogEmitter{})

	// This routine may log; start it after initializing syslog.
	InstallThreadProfiles(ctx, componentCtx)

	_ = syslog.Infof("starting...")

	var opaqueIIDOpts ipv6.OpaqueInterfaceIdentifierOptions
	if !noOpaqueIID {
		secretKeyForOpaqueIID, err := getSecretKeyForOpaqueIID(componentCtx)
		if err != nil {
			panic(fmt.Sprintf("failed to get secret key for opaque IIDs: %s", err))
		}
		opaqueIIDOpts = ipv6.OpaqueInterfaceIdentifierOptions{
			NICNameFromID: func(nicID tcpip.NICID, nicName string) string {
				// As of writing, Netstack creates NICs with names so we return the name
				// the NIC was created with. Just in case, we have a default NIC name
				// format for NICs that were not created with a name.
				if nicName != "" {
					return nicName
				}
				return fmt.Sprintf("opaqueIIDNIC%d", nicID)
			},
			SecretKey: secretKeyForOpaqueIID,
		}
	}

	tempIIDSeed, err := newSecretKey(header.IIDSize)
	if err != nil {
		panic(fmt.Sprintf("failed to get temp IID seed: %s", err))
	}

	ndpDisp := newNDPDispatcher()
	nudDisp := newNudDispatcher()

	dadConfigs := tcpipstack.DADConfigurations{
		DupAddrDetectTransmits: dadTransmits,
		RetransmitTimer:        dadRetransmitTimer,
	}

	stk := tcpipstack.New(tcpipstack.Options{
		NetworkProtocols: []tcpipstack.NetworkProtocolFactory{
			arp.NewProtocolWithOptions(arp.Options{
				DADConfigs: dadConfigs,
			}),
			ipv4.NewProtocolWithOptions(ipv4.Options{
				IGMP: ipv4.IGMPOptions{
					Enabled: true,
				},
			}),
			ipv6.NewProtocolWithOptions(ipv6.Options{
				DADConfigs: dadConfigs,
				NDPConfigs: ipv6.NDPConfigurations{
					MaxRtrSolicitations:           maxRtrSolicitations,
					RtrSolicitationInterval:       rtrSolicitationInterval,
					MaxRtrSolicitationDelay:       maxRtrSolicitationDelay,
					HandleRAs:                     handleRAs,
					DiscoverDefaultRouters:        true,
					DiscoverMoreSpecificRoutes:    true,
					DiscoverOnLinkPrefixes:        true,
					AutoGenGlobalAddresses:        true,
					AutoGenAddressConflictRetries: autoGenAddressConflictRetries,
					AutoGenTempGlobalAddresses:    true,
					MaxTempAddrValidLifetime:      maxTempAddrValidLifetime,
					MaxTempAddrPreferredLifetime:  maxTempAddrPreferredLifetime,
					RegenAdvanceDuration:          regenAdvanceDuration,
				},
				AutoGenLinkLocal: true,
				NDPDisp:          ndpDisp,
				OpaqueIIDOpts:    opaqueIIDOpts,
				TempIIDSeed:      tempIIDSeed,
				MLD: ipv6.MLDOptions{
					Enabled: true,
				},
			}),
		},
		TransportProtocols: []tcpipstack.TransportProtocolFactory{
			icmp.NewProtocol4,
			icmp.NewProtocol6,
			tcp.NewProtocol,
			udp.NewProtocol,
		},
		HandleLocal: true,
		NUDDisp:     nudDisp,

		RawFactory:               &raw.EndpointFactory{},
		AllowPacketEndpointWrite: true,
		Clock:                    &zxtime.Clock{},
	})

	delayEnabled := tcpip.TCPDelayEnabled(true)
	sackEnabled := tcpip.TCPSACKEnabled(true)
	moderateReceiveBufferOption := tcpip.TCPModerateReceiveBufferOption(true)
	for _, opt := range []tcpip.SettableTransportProtocolOption{
		&delayEnabled,
		&sackEnabled,
		&moderateReceiveBufferOption,
	} {
		if err := stk.SetTransportProtocolOption(tcp.ProtocolNumber, opt); err != nil {
			syslog.Fatalf("SetTransportProtocolOption(%d, %#v) failed: %s", tcp.ProtocolNumber, opt, err)
		}
	}

	f := filter.New(stk, &filterNicInfoProvider{stack: stk})

	interfaceEventChan := make(chan interfaceEvent)
	interfacesWatcherChan := make(chan interfaceWatcherRequest)
	fidlInterfaceWatcherStats := &fidlInterfaceWatcherStats{}
	go interfaceWatcherEventLoop(ctx, interfaceEventChan, interfacesWatcherChan, fidlInterfaceWatcherStats)

	interruptChan := make(chan routeInterrupt, maxPendingInterrupts)
	fidlRoutesWatcherMetrics := &fidlRoutesWatcherMetrics{}
	go routesWatcherEventLoop(ctx, interruptChan, fidlRoutesWatcherMetrics)

	ns := &Netstack{
		interfaceEventChan: interfaceEventChan,
		routeTable: routes.NewRouteTableWithOnChangeCallback(
			func(c routes.RoutingTableChange) {
				interruptChan <- &routingTableChange{c}
			},
		),
		dnsConfig:          dns.MakeServersConfig(stk.Clock()),
		stack:              stk,
		stats:              stats{Stats: stk.Stats()},
		nicRemovedHandlers: []NICRemovedHandler{&ndpDisp.dynamicAddressSourceTracker, f},
		featureFlags:       featureFlags{enableFastUDP: fastUDP},
	}

	fastUDPStatus := "disabled"
	if fastUDP {
		fastUDPStatus = "enabled"
	}
	_ = syslog.Infof("fast UDP is %s", fastUDPStatus)
	ns.resetDestinationCache()

	nudDisp.ns = ns
	ndpDisp.ns = ns
	ndpDisp.dynamicAddressSourceTracker.init(ns)

	filter.AddOutgoingService(componentCtx, f)

	{
		if err := ns.addLoopback(); err != nil {
			syslog.Fatalf("loopback: %s", err)
		}
		// Handle all of the already-enqueued NDP events so that DAD
		// completion for ::1 is observed. This ensures that clients
		// to the interface watcher are guaranteed to observe ::1 in
		// the Existing event rather than as a separate Changed event.
		for {
			event := func() ndpEvent {
				ndpDisp.mu.Lock()
				defer ndpDisp.mu.Unlock()

				if len(ndpDisp.mu.events) == 0 {
					return nil
				}
				return ndpDisp.mu.events[0]
			}()
			if event == nil {
				break
			}
			ndpDisp.handleEvent(event)
		}
	}

	ndpDisp.start(ctx)

	dnsWatchers := newDnsServerWatcherCollection(ns.dnsConfig.GetServersCacheAndChannel)

	if err := tracingprovider.Create(componentCtx); err != nil {
		syslog.Warnf("could not create a trace provider: %s", err)
		// Trace manager can not be running, or not available in the namespace. We can continue.
	}

	// The node at /root/fuchsia.inspect.Health is important for testing purposes
	// as it's the only node that is common to both NS2 and NS3.
	componentCtx.OutgoingService.AddDiagnostics("root", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			asService: (&inspectImpl{
				inner: &rootInspectImpl{
					name: "root",
					health: healthInspectImpl{
						name: "fuchsia.inspect.Health",
					},
				},
			}).asService,
		},
	})
	componentCtx.OutgoingService.AddDiagnostics("configuration", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			asService: (&inspectImpl{
				inner: &configInspectImpl{
					name:                   "Runtime Configuration Flags",
					logPackets:             &logPackets,
					logLevel:               logLevel,
					socketStatsTimerPeriod: socketStatsTimerPeriod,
					noOpaqueIID:            noOpaqueIID,
					fastUDP:                fastUDP,
					config: configDataInspectImpl{
						name:   "config-data",
						file:   *configFile,
						data:   config,
						numCPU: runtime.NumCPU(),
					},
				},
			}).asService,
		},
	})
	componentCtx.OutgoingService.AddDiagnostics("counters", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			asService: (&inspectImpl{
				inner: &networkingStatCountersInspectImpl{
					statCounterInspectImpl: statCounterInspectImpl{
						name:  "Networking Stat Counters",
						value: reflect.ValueOf(&ns.stats).Elem(),
					},
					ns: ns,
				},
			}).asService,
		},
	})
	componentCtx.OutgoingService.AddDiagnostics("interfaces", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			// asService is late-bound so that each call retrieves fresh NIC info.
			asService: func() *component.Service {
				return (&inspectImpl{
					inner: &nicInfoMapInspectImpl{value: ns.getIfStateInfo(stk.NICInfo())},
				}).asService()
			},
		},
	})
	componentCtx.OutgoingService.AddDiagnostics("fidlStats", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			asService: (&inspectImpl{
				inner: &fidlStatsInspectImpl{
					name:                      "Networking FIDL Protocol Stats",
					fidlInterfaceWatcherStats: fidlInterfaceWatcherStats,
					fidlRoutesWatcherMetrics:  fidlRoutesWatcherMetrics,
				},
			}).asService,
		},
	})
	componentCtx.OutgoingService.AddDiagnostics("sockets", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			asService: (&inspectImpl{
				inner: &socketInfoMapInspectImpl{
					value: &ns.endpoints,
				},
			}).asService,
		},
	})
	componentCtx.OutgoingService.AddDiagnostics("routes", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			// asService is late-bound so that each call retrieves fresh routing table info.
			asService: func() *component.Service {
				return (&inspectImpl{
					inner: &routingTableInspectImpl{value: ns.GetExtendedRouteTable()},
				}).asService()
			},
		},
	})
	componentCtx.OutgoingService.AddDiagnostics("memstats", &component.DirectoryWrapper{
		Directory: &inspectDirectory{
			// asService is late-bound so that each call retrieves fresh stats.
			asService: func() *component.Service {
				return (&inspectImpl{
					inner: &memstatsInspectImpl{},
				}).asService()
			},
		},
	})

	// Minimal support for the inspect VMO format allows our profile protos to be
	// picked up by bug reports.
	//
	// To extract these serialized protos from inspect.json, jq can be used:
	//
	// cat iquery.json | \
	// jq '.[] | select(.path | contains("/pprof/")) | .contents.root.pprof.goroutine[4:]' | \
	// xargs echo | base64 --decode > goroutine
	componentCtx.OutgoingService.AddDiagnostics("pprof", pprof.NewNode())

	// Periodically request that the persistence service save counters. This,
	// along with persisted logs, makes it easier to debug connectivity issues
	// before a reboot.
	startPersistClient(ctx, componentCtx, stk.Clock())

	{
		stub := verify.ComponentOtaHealthCheckWithCtxStub{Impl: &healthCheck{}}
		componentCtx.OutgoingService.AddService(
			verify.ComponentOtaHealthCheckName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(verify.ComponentOtaHealthCheckName, "%s", err)
					},
				})

				return nil
			},
		)
	}
	{
		tokenV4, err := zx.NewEvent(0 /* options */)
		if err != nil {
			panic("cannot initialize a zircon event")
		}
		stubV4 := routesAdmin.RouteTableV4WithCtxStub{
			Impl: &routesAdminRouteTableV4Impl{
				routesAdminMainRouteTable: routesAdminMainRouteTable{
					tableId: fidlconv.V4MainRouteTableId,
					token:   tokenV4,
				},
				ns: ns,
			},
		}
		tokenV6, err := zx.NewEvent(0 /* options */)
		if err != nil {
			panic("cannot initialize a zircon event")
		}
		stubV6 := routesAdmin.RouteTableV6WithCtxStub{
			Impl: &routesAdminRouteTableV6Impl{
				routesAdminMainRouteTable: routesAdminMainRouteTable{
					tableId: fidlconv.V6MainRouteTableId,
					token:   tokenV6,
				},
				ns: ns,
			},
		}
		componentCtx.OutgoingService.AddService(
			routesAdmin.RouteTableV4Name,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stubV4, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.ErrorTf(routesAdmin.RouteTableV4Name, "%s", err)
					},
				})
				return nil
			},
		)
		componentCtx.OutgoingService.AddService(
			routesAdmin.RouteTableV6Name,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stubV6, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.ErrorTf(routesAdmin.RouteTableV6Name, "%s", err)
					},
				})
				return nil
			},
		)
		stub := stack.StackWithCtxStub{Impl: &stackImpl{
			ns: ns,
		}}
		componentCtx.OutgoingService.AddService(
			stack.StackName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(stack.StackName, "%s", err)
					},
				})
				return nil
			},
		)
	}

	componentCtx.OutgoingService.AddService(
		name.DnsServerWatcherName,
		dnsWatchers.bind,
	)

	{
		stub := stack.LogWithCtxStub{Impl: &logImpl{
			logPackets: &sniffer.LogPackets,
		}}
		componentCtx.OutgoingService.AddService(
			stack.LogName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(stack.LogName, "%s", err)
					},
				})
				return nil
			})
	}

	{
		stub := socket.ProviderWithCtxStub{Impl: &providerImpl{ns: ns}}
		componentCtx.OutgoingService.AddService(
			socket.ProviderName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(socket.ProviderName, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		stub := rawsocket.ProviderWithCtxStub{Impl: &rawProviderImpl{ns: ns}}
		componentCtx.OutgoingService.AddService(
			rawsocket.ProviderName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(rawsocket.ProviderName, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		stub := packetsocket.ProviderWithCtxStub{Impl: &packetProviderImpl{ns: ns}}
		componentCtx.OutgoingService.AddService(
			packetsocket.ProviderName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(packetsocket.ProviderName, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		resolveImpl := resolveImpl{
			stack: ns.stack,
		}

		stub := fnetRoutes.StateWithCtxStub{Impl: &resolveImpl}
		componentCtx.OutgoingService.AddService(
			fnetRoutes.StateName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(fnetRoutes.StateName, "%s", err)
					},
				})
				return nil
			},
		)
		getWatcherImpl := getWatcherImpl{
			interruptChan: interruptChan,
		}

		stub_v4 := fnetRoutes.StateV4WithCtxStub{Impl: &getWatcherImpl}
		componentCtx.OutgoingService.AddService(
			fnetRoutes.StateV4Name,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub_v4, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(fnetRoutes.StateV4Name, "%s", err)
					},
				})
				return nil
			},
		)

		stub_v6 := fnetRoutes.StateV6WithCtxStub{Impl: &getWatcherImpl}
		componentCtx.OutgoingService.AddService(
			fnetRoutes.StateV6Name,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub_v6, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(fnetRoutes.StateV6Name, "%s", err)
					},
				})
				return nil
			},
		)

	}

	{
		stub := interfaces.StateWithCtxStub{Impl: &interfaceStateImpl{watcherChan: interfacesWatcherChan}}
		componentCtx.OutgoingService.AddService(
			interfaces.StateName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(interfaces.StateName, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		stub := admin.InstallerWithCtxStub{Impl: &interfacesAdminInstallerImpl{ns: ns}}
		componentCtx.OutgoingService.AddService(
			admin.InstallerName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(admin.InstallerName, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		stub := debug.InterfacesWithCtxStub{Impl: &debugInterfacesImpl{ns: ns}}
		componentCtx.OutgoingService.AddService(
			debug.InterfacesName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(debug.InterfacesName, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		stub := root.InterfacesWithCtxStub{Impl: &rootInterfacesImpl{ns: ns}}
		componentCtx.OutgoingService.AddService(
			root.InterfacesName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(debug.InterfacesName, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		stub := root.RoutesV4WithCtxStub{Impl: &rootRoutesV4Impl{ns: ns}}
		componentCtx.OutgoingService.AddService(
			root.RoutesV4Name,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(root.RoutesV4Name, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		stub := root.RoutesV6WithCtxStub{Impl: &rootRoutesV6Impl{ns: ns}}
		componentCtx.OutgoingService.AddService(
			root.RoutesV6Name,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(root.RoutesV6Name, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		stub := debug.DiagnosticsWithCtxStub{Impl: &debugDiagnosticsImpl{}}
		componentCtx.OutgoingService.AddService(
			debug.DiagnosticsName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &stub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(debug.DiagnosticsName, "%s", err)
					},
				})
				return nil
			},
		)
	}

	{
		impl := newNeighborImpl(stk)

		viewStub := neighbor.ViewWithCtxStub{Impl: impl}
		componentCtx.OutgoingService.AddService(
			neighbor.ViewName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &viewStub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(neighbor.ViewName, "%s", err)
					},
				})
				return nil
			},
		)

		controllerStub := neighbor.ControllerWithCtxStub{Impl: impl}
		componentCtx.OutgoingService.AddService(
			neighbor.ControllerName,
			func(ctx context.Context, c zx.Channel) error {
				go component.Serve(ctx, &controllerStub, c, component.ServeOptions{
					OnError: func(err error) {
						_ = syslog.WarnTf(neighbor.ControllerName, "%s", err)
					},
				})
				return nil
			},
		)

		go impl.observeEvents(nudDisp.events)
	}

	addMulticastIpv4RoutingTableControllerService(componentCtx, ns.stack)
	addMulticastIpv6RoutingTableControllerService(componentCtx, ns.stack)

	{
		componentCtx.OutgoingService.AddService(
			routesAdmin.RouteTableProviderV4Name,
			func(ctx context.Context, c zx.Channel) error {
				_ = syslog.ErrorTf(routesAdmin.RouteTableProviderV4Name,
					"netstack2 does not support %s",
					routesAdmin.RouteTableProviderV4Name,
				)
				component.CloseWithEpitaph(c, zx.ErrNotSupported)
				return nil
			},
		)

		componentCtx.OutgoingService.AddService(
			routesAdmin.RouteTableProviderV6Name,
			func(ctx context.Context, c zx.Channel) error {
				_ = syslog.ErrorTf(routesAdmin.RouteTableProviderV6Name,
					"netstack2 does not support %s",
					routesAdmin.RouteTableProviderV6Name,
				)
				component.CloseWithEpitaph(c, zx.ErrNotSupported)
				return nil
			},
		)

		componentCtx.OutgoingService.AddService(
			routesAdmin.RuleTableV4Name,
			func(ctx context.Context, c zx.Channel) error {
				_ = syslog.ErrorTf(routesAdmin.RuleTableV4Name,
					"netstack2 does not support %s",
					routesAdmin.RuleTableV4Name,
				)
				component.CloseWithEpitaph(c, zx.ErrNotSupported)
				return nil
			},
		)

		componentCtx.OutgoingService.AddService(
			routesAdmin.RuleTableV6Name,
			func(ctx context.Context, c zx.Channel) error {
				_ = syslog.ErrorTf(routesAdmin.RuleTableV6Name,
					"netstack2 does not support %s",
					routesAdmin.RuleTableV6Name,
				)
				component.CloseWithEpitaph(c, zx.ErrNotSupported)
				return nil
			},
		)

		componentCtx.OutgoingService.AddService(
			fnetNdp.RouterAdvertisementOptionWatcherProviderName,
			func(ctx context.Context, c zx.Channel) error {
				_ = syslog.WarnTf(fnetNdp.RouterAdvertisementOptionWatcherProviderName,
					"netstack2 does not support %s",
					fnetNdp.RouterAdvertisementOptionWatcherProviderName)
				component.CloseWithEpitaph(c, zx.ErrNotSupported)
				return nil
			},
		)
	}

	componentCtx.BindStartupHandle(context.Background())
}

// newSecretKey returns a new secret key.
func newSecretKey(keyLen int) ([]byte, error) {
	secretKey := make([]byte, keyLen)
	if _, err := io.ReadFull(rand.Reader, secretKey); err != nil {
		return nil, fmt.Errorf("failed to populate a new secret key of %d bytes: %s", keyLen, err)
	}
	return secretKey, nil
}

// newSecretKeyForOpaqueIID returns a new secret key for opaque IID generation.
func newSecretKeyForOpaqueIID() ([]byte, error) {
	return newSecretKey(header.OpaqueIIDSecretKeyMinBytes)
}

// getSecretKeyForOpaqueIID gets a secret key for opaque IID generation from the
// secure stash store service, or attempts to create one. If the stash service
// is unavailable, a temporary secret key will be returned.
func getSecretKeyForOpaqueIID(componentCtx *component.Context) ([]byte, error) {
	syslog.VLogf(syslog.DebugVerbosity, "getting or creating secret key for opaque IID from secure stash store")

	// Connect to the secure stash store service.
	storeReq, store, err := stash.NewSecureStoreWithCtxInterfaceRequest()
	if err != nil {
		syslog.Errorf("could not create the request to connect to the %s service: %s", stash.SecureStoreName, err)
		return newSecretKeyForOpaqueIID()
	}
	defer func() {
		_ = store.Close()
	}()
	componentCtx.ConnectToEnvService(storeReq)

	// Use our secure stash.
	if err := store.Identify(context.Background(), stashStoreIdentificationName); err != nil {
		syslog.Warnf("failed to identify as %s to the secure stash store: %s", stashStoreIdentificationName, err)
		return newSecretKeyForOpaqueIID()
	}
	storeAccessorReq, storeAccessor, err := stash.NewStoreAccessorWithCtxInterfaceRequest()
	if err != nil {
		syslog.Errorf("could not create the secure stash store accessor request: %s", err)
		return newSecretKeyForOpaqueIID()
	}
	defer func() {
		_ = storeAccessor.Close()
	}()
	if err := store.CreateAccessor(context.Background(), false /* readOnly */, storeAccessorReq); err != nil {
		syslog.Warnf("failed to create accessor to the secure stash store: %s", err)
		return newSecretKeyForOpaqueIID()
	}

	// Attempt to get the existing secret key.
	opaqueIIDSecretKeyValue, err := storeAccessor.GetValue(context.Background(), opaqueIIDSecretKeyName)
	if err != nil {
		syslog.Warnf("failed to get opaque IID secret key from secure stash store: %s", err)
		return newSecretKeyForOpaqueIID()
	}

	// If a key exists, make sure it is valid before returning it.
	//
	// The value should be stored as a base64 encoded string.
	//
	// We use a string because stash.Value.Bytesval uses a fuchsia.mem.Buffer
	// which uses a VMO (which uses memory in page size increments). This is
	// wasteful as the key only uses 16 bytes. We base64 encode the string
	// because stash.Value.Stringval expects an ascii string; the store returns an
	// error when flushing the store with a stash.Value.Stringval of raw bytes.
	if opaqueIIDSecretKeyValue != nil && opaqueIIDSecretKeyValue.Which() == stash.ValueStringval {
		syslog.VLogf(syslog.DebugVerbosity, "found a secret key for opaque IIDs in the secure stash store")

		if secretKey, err := base64.StdEncoding.DecodeString(opaqueIIDSecretKeyValue.Stringval); err != nil {
			syslog.Errorf("failed to decode the secret key string: %s", err)
		} else if l := len(secretKey); l != header.OpaqueIIDSecretKeyMinBytes {
			syslog.Errorf("invalid secret key for opaque IIDs; got length = %d, want = %d", l, header.OpaqueIIDSecretKeyMinBytes)
		} else {
			syslog.VLogf(syslog.DebugVerbosity, "using existing secret key for opaque IIDs")
			return secretKey, nil
		}
	}

	// Generate a new secret key as we either do not have one or the one we have
	// is invalid.
	syslog.VLogf(syslog.DebugVerbosity, "generating a new secret key for opaque IIDs")
	secretKey, err := newSecretKeyForOpaqueIID()
	if err != nil {
		return nil, err
	}

	// Store the newly generated key to the secure stash store as a base64
	// encoded string.
	if err := storeAccessor.SetValue(context.Background(), opaqueIIDSecretKeyName, stash.ValueWithStringval(base64.StdEncoding.EncodeToString(secretKey))); err != nil {
		syslog.Errorf("failed to set newly created secret key for opaque IID to secure stash store: %s", err)
		return secretKey, nil
	}
	flushResp, err := storeAccessor.Flush(context.Background())
	if err != nil {
		syslog.Errorf("failed to flush secure stash store with updated secret key for opaque IID: %s", err)
		return secretKey, nil
	}
	switch w := flushResp.Which(); w {
	case stash.StoreAccessorFlushResultErr:
		syslog.Errorf("got error response when flushing secure stash store with updated secret key for opaque IID: %s", flushResp.Err)
		return secretKey, nil

	case stash.StoreAccessorFlushResultResponse:
		syslog.VLogf(syslog.DebugVerbosity, "saved newly generated secret key for opaque IIDs to secure stash store")
		return secretKey, nil

	default:
		panic(fmt.Sprintf("unexpected store accessor flush result type: %d", w))
	}
}

var _ verify.ComponentOtaHealthCheckWithCtx = (*healthCheck)(nil)

type healthCheck struct{}

func (*healthCheck) GetHealthStatus(fidl.Context) (verify.HealthStatus, error) {
	// Wait an arbitrary amount of time; if we didn't crash, we're probably healthy.
	<-time.After(15 * time.Second)
	return verify.HealthStatusHealthy, nil
}
