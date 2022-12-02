// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_HOST_DEVFS_VNODE_H_
#define SRC_DEVICES_BIN_DRIVER_HOST_DEVFS_VNODE_H_

#include <fidl/fuchsia.device/cpp/wire.h>
#include <lib/fidl-async/cpp/bind.h>
#include <lib/fidl/cpp/wire/transaction.h>

#include <variant>

#include <ddktl/fidl.h>

#include "src/lib/storage/vfs/cpp/vnode.h"

struct zx_device;

class DevfsVnode : public fs::Vnode, public fidl::WireServer<fuchsia_device::Controller> {
 public:
  DevfsVnode(fbl::RefPtr<zx_device> dev, async_dispatcher_t* dispatcher)
      : dev_(std::move(dev)), dispatcher_(dispatcher) {}

  // fs::Vnode methods
  zx_status_t GetAttributes(fs::VnodeAttributes* a) override;
  fs::VnodeProtocolSet GetProtocols() const override;
  zx_status_t GetNodeInfoForProtocol(fs::VnodeProtocol protocol, fs::Rights rights,
                                     fs::VnodeRepresentation* info) override;
  void HandleFsSpecificMessage(fidl::IncomingHeaderAndMessage& msg,
                               fidl::Transaction* txn) override;
  void ConnectToDeviceFidl(zx::channel server);

  // fidl::WireServer<fuchsia_device::Controller> methods
  void ConnectToDeviceFidl(ConnectToDeviceFidlRequestView request,
                           ConnectToDeviceFidlCompleter::Sync& completer) override;
  void Bind(BindRequestView request, BindCompleter::Sync& _completer) override;
  void Rebind(RebindRequestView request, RebindCompleter::Sync& _completer) override;
  void UnbindChildren(UnbindChildrenCompleter::Sync& completer) override;
  void ScheduleUnbind(ScheduleUnbindCompleter::Sync& _completer) override;
  void GetTopologicalPath(GetTopologicalPathCompleter::Sync& _completer) override;
  void GetMinDriverLogSeverity(GetMinDriverLogSeverityCompleter::Sync& _completer) override;
  void GetCurrentPerformanceState(GetCurrentPerformanceStateCompleter::Sync& completer) override;
  void SetMinDriverLogSeverity(SetMinDriverLogSeverityRequestView request,
                               SetMinDriverLogSeverityCompleter::Sync& _completer) override;
  void SetPerformanceState(SetPerformanceStateRequestView request,
                           SetPerformanceStateCompleter::Sync& _completer) override;

 private:
  // Vnode protected implementation:
  zx_status_t OpenNode(fs::Vnode::ValidatedOptions options,
                       fbl::RefPtr<Vnode>* out_redirect) override;
  zx_status_t CloseNode() override;

  fbl::RefPtr<zx_device> dev_;
  async_dispatcher_t* dispatcher_;
};

#endif  // SRC_DEVICES_BIN_DRIVER_HOST_DEVFS_VNODE_H_
