// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_MISC_DRIVERS_COMPAT_DEVFS_VNODE_H_
#define SRC_DEVICES_MISC_DRIVERS_COMPAT_DEVFS_VNODE_H_

#include <fidl/fuchsia.device/cpp/wire.h>
#include <lib/fidl/cpp/wire/transaction.h>

#include <variant>

#include <ddktl/fidl.h>

#include "src/lib/storage/vfs/cpp/vnode.h"

class DevfsVnode : public fs::Vnode, public fidl::WireServer<fuchsia_device::Controller> {
 public:
  // Create a DevfsVnode. `dev` is unowned, so the Device must outlive the Vnode.
  explicit DevfsVnode(zx_device* dev) : dev_(dev) {}

  // fs::Vnode methods
  zx_status_t GetAttributes(fs::VnodeAttributes* a) override;
  fs::VnodeProtocolSet GetProtocols() const override;
  zx_status_t GetNodeInfoForProtocol(fs::VnodeProtocol protocol, fs::Rights rights,
                                     fs::VnodeRepresentation* info) override;
  void HandleFsSpecificMessage(fidl::IncomingHeaderAndMessage& msg,
                               fidl::Transaction* txn) override;

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

  zx_device* dev() { return dev_; }

 private:
  // Vnode protected implementation:
  zx_status_t OpenNode(fs::Vnode::ValidatedOptions options,
                       fbl::RefPtr<Vnode>* out_redirect) override;
  zx_status_t CloseNode() override;

  // A pointer to the device that this vnode represents. This will be
  // set to nullptr if the device is freed.
  zx_device* dev_;
};

#endif  // SRC_DEVICES_MISC_DRIVERS_COMPAT_DEVFS_VNODE_H_
