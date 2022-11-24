// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_host/devfs_vnode.h"

#include <lib/async/default.h>
#include <lib/ddk/device.h>

#include <string_view>

#include <fbl/string_buffer.h>

#include "src/devices/bin/driver_host/driver_host_context.h"
#include "src/devices/bin/driver_host/simple_binding.h"
#include "src/devices/bin/driver_host/zx_device.h"
#include "src/devices/lib/log/log.h"
#include "src/lib/storage/vfs/cpp/vfs_types.h"

namespace {
// Utility class for dispatching messages to a device.
class FidlDispatcher : public fidl::internal::IncomingMessageDispatcher {
 public:
  explicit FidlDispatcher(fbl::RefPtr<zx_device> dev) : dev_(std::move(dev)) {}

  static void CreateAndBind(fbl::RefPtr<zx_device> dev, async_dispatcher_t* dispatcher,
                            zx::channel channel);

 private:
  void dispatch_message(fidl::IncomingHeaderAndMessage&& msg, ::fidl::Transaction* txn,
                        fidl::internal::MessageStorageViewBase* storage_view) final;
  fbl::RefPtr<zx_device> dev_;
};

void FidlDispatcher::CreateAndBind(fbl::RefPtr<zx_device> dev, async_dispatcher_t* dispatcher,
                                   zx::channel channel) {
  auto fidl = std::make_unique<FidlDispatcher>(std::move(dev));
  auto fidl_ptr = fidl.get();

  // Create the binding. We pass the FidlDispatcher's pointer into the unbound
  // function so it stays alive as long as the binding.
  auto binding = std::make_unique<devfs::SimpleBinding>(dispatcher, std::move(channel), fidl_ptr,
                                                        [fidl = std::move(fidl)](void*) {});
  zx_status_t status = devfs::BeginWait(&binding);
  if (status != ZX_OK) {
    LOGF(ERROR, "FidlDispatcher: Failed to BeginWait: %s", zx_status_get_string(status));
  }
}

void FidlDispatcher::dispatch_message(fidl::IncomingHeaderAndMessage&& msg,
                                      ::fidl::Transaction* txn,
                                      fidl::internal::MessageStorageViewBase* storage_view) {
  // If the device is unbound it shouldn't receive messages so close the channel.
  if (dev_->Unbound()) {
    txn->Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }

  fidl_incoming_msg_t c_msg = std::move(msg).ReleaseToEncodedCMessage();
  auto ddk_txn = MakeDdkInternalTransaction(txn);
  zx_status_t status = dev_->MessageOp(&c_msg, ddk_txn.Txn());
  if (status != ZX_OK && status != ZX_ERR_ASYNC) {
    // Close the connection on any error
    txn->Close(status);
  }
}

}  // namespace

zx_status_t DevfsVnode::OpenNode(fs::Vnode::ValidatedOptions options,
                                 fbl::RefPtr<Vnode>* out_redirect) {
  if (dev_->Unbound()) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  fbl::RefPtr<zx_device_t> new_dev;
  zx_status_t status = device_open(dev_, &new_dev, static_cast<uint32_t>(options->ToIoV1Flags()));
  if (status != ZX_OK) {
    return status;
  }
  if (new_dev != dev_) {
    *out_redirect = new_dev->vnode;
  }
  return ZX_OK;
}

zx_status_t DevfsVnode::CloseNode() {
  zx_status_t status = device_close(dev_, 0);
  // If this vnode is for an instance device, drop its reference on close to break
  // the reference cycle.  This is handled for non-instance devices during the device
  // remove path.
  if (dev_->flags() & DEV_FLAG_INSTANCE) {
    dev_->vnode.reset();
  }
  return status;
}

zx_status_t DevfsVnode::GetAttributes(fs::VnodeAttributes* a) {
  if (dev_->Unbound()) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  a->mode = V_TYPE_CDEV | V_IRUSR | V_IWUSR;
  a->content_size = dev_->GetSizeOp();
  a->link_count = 1;
  return ZX_OK;
}

fs::VnodeProtocolSet DevfsVnode::GetProtocols() const { return fs::VnodeProtocol::kFile; }

zx_status_t DevfsVnode::GetNodeInfoForProtocol(fs::VnodeProtocol protocol, fs::Rights rights,
                                               fs::VnodeRepresentation* info) {
  if (protocol == fs::VnodeProtocol::kFile) {
    *info = fs::VnodeRepresentation::File{};
    return ZX_OK;
  }
  return ZX_ERR_NOT_SUPPORTED;
}

void DevfsVnode::ConnectToDeviceFidl(zx::channel server) {
  FidlDispatcher::CreateAndBind(dev_, dispatcher_, std::move(server));
}

void DevfsVnode::ConnectToDeviceFidl(ConnectToDeviceFidlRequestView request,
                                     ConnectToDeviceFidlCompleter::Sync& completer) {
  ConnectToDeviceFidl(std::move(request->server));
}

void DevfsVnode::HandleFsSpecificMessage(fidl::IncomingHeaderAndMessage& msg,
                                         fidl::Transaction* txn) {
  if (dev_->Unbound()) {
    txn->Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  ::fidl::DispatchResult dispatch_result =
      fidl::WireTryDispatch<fuchsia_device::Controller>(this, msg, txn);
  if (dispatch_result == ::fidl::DispatchResult::kFound) {
    return;
  }

  fidl_incoming_msg_t c_msg = std::move(msg).ReleaseToEncodedCMessage();
  auto ddk_txn = MakeDdkInternalTransaction(txn);
  zx_status_t status = dev_->MessageOp(&c_msg, ddk_txn.Txn());
  if (status != ZX_OK && status != ZX_ERR_ASYNC) {
    // Close the connection on any error
    txn->Close(status);
  }
}

void DevfsVnode::Bind(BindRequestView request, BindCompleter::Sync& completer) {
  if (dev_->Unbound()) {
    completer.Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  zx_status_t status =
      device_bind(dev_, std::string(request->driver.data(), request->driver.size()).c_str());
  if (status != ZX_OK) {
    completer.ReplyError(status);
  } else {
    dev_->set_bind_conn([completer = completer.ToAsync()](zx_status_t status) mutable {
      if (status != ZX_OK) {
        completer.ReplyError(status);
      } else {
        completer.ReplySuccess();
      }
    });
  }
}

void DevfsVnode::GetCurrentPerformanceState(GetCurrentPerformanceStateCompleter::Sync& completer) {
  if (dev_->Unbound()) {
    completer.Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  completer.Reply(dev_->current_performance_state());
}

void DevfsVnode::Rebind(RebindRequestView request, RebindCompleter::Sync& completer) {
  if (dev_->Unbound()) {
    completer.Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  dev_->set_rebind_drv_name(std::string(request->driver.data(), request->driver.size()).c_str());
  zx_status_t status = device_rebind(dev_.get());

  if (status != ZX_OK) {
    completer.ReplyError(status);
  } else {
    // These will be set, until device is unbound and then bound again.
    dev_->set_rebind_conn([completer = completer.ToAsync()](zx_status_t status) mutable {
      if (status != ZX_OK) {
        completer.ReplyError(status);
      } else {
        completer.ReplySuccess();
      }
    });
  }
}

void DevfsVnode::UnbindChildren(UnbindChildrenCompleter::Sync& completer) {
  if (dev_->Unbound()) {
    completer.Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  zx::result<bool> scheduled_unbind = device_schedule_unbind_children(dev_);
  if (scheduled_unbind.is_error()) {
    completer.ReplyError(scheduled_unbind.status_value());
  }

  // Handle case where we have no children to unbind (otherwise the callback below will never fire).
  if (!scheduled_unbind.value()) {
    completer.ReplySuccess();
    return;
  }

  // Asynchronously respond to the unbind request once all children have been unbound.
  // The unbind children conn will be set until all the children of this device are unbound.
  dev_->set_unbind_children_conn([completer = completer.ToAsync()](zx_status_t status) mutable {
    if (status != ZX_OK) {
      completer.ReplyError(status);
    } else {
      completer.ReplySuccess();
    }
  });
}

void DevfsVnode::ScheduleUnbind(ScheduleUnbindCompleter::Sync& completer) {
  if (dev_->Unbound()) {
    completer.Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  zx_status_t status = device_schedule_remove(dev_, true /* unbind_self */);
  if (status != ZX_OK) {
    completer.ReplyError(status);
  } else {
    completer.ReplySuccess();
  }
}

void DevfsVnode::GetTopologicalPath(GetTopologicalPathCompleter::Sync& completer) {
  if (dev_->Unbound()) {
    completer.Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  char buf[fuchsia_device::wire::kMaxDevicePathLen + 1];
  size_t actual;
  zx_status_t status = dev_->driver_host_context()->GetTopoPath(dev_, buf, sizeof(buf), &actual);
  if (status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  if (actual > 0) {
    // Remove the accounting for the null byte
    actual--;
  }
  auto path = ::fidl::StringView(buf, actual);
  completer.ReplySuccess(path);
}

void DevfsVnode::GetMinDriverLogSeverity(GetMinDriverLogSeverityCompleter::Sync& completer) {
  if (dev_->Unbound()) {
    completer.Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  if (!dev_->driver) {
    completer.Reply(ZX_ERR_UNAVAILABLE, fuchsia_logger::wire::LogLevelFilter::kNone);
    return;
  }
  fx_log_severity_t severity = fx_logger_get_min_severity(dev_->zx_driver()->logger());
  completer.Reply(ZX_OK, static_cast<fuchsia_logger::wire::LogLevelFilter>(severity));
}

void DevfsVnode::SetMinDriverLogSeverity(SetMinDriverLogSeverityRequestView request,
                                         SetMinDriverLogSeverityCompleter::Sync& completer) {
  if (dev_->Unbound()) {
    completer.Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  if (!dev_->driver) {
    completer.Reply(ZX_ERR_UNAVAILABLE);
    return;
  }
  auto status = dev_->zx_driver()->set_driver_min_log_severity(
      static_cast<fx_log_severity_t>(request->severity));
  completer.Reply(status);
}

void DevfsVnode::SetPerformanceState(SetPerformanceStateRequestView request,
                                     SetPerformanceStateCompleter::Sync& completer) {
  if (dev_->Unbound()) {
    completer.Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  uint32_t out_state;
  zx_status_t status = dev_->driver_host_context()->DeviceSetPerformanceState(
      dev_, request->requested_state, &out_state);
  completer.Reply(status, out_state);
}

namespace {

// Reply originating from driver.
zx_status_t DdkReply(fidl_txn_t* txn, const fidl_outgoing_msg_t* msg) {
  auto message = fidl::OutgoingMessage::FromEncodedCMessage(msg);
  // If FromDdkInternalTransaction returns a unique_ptr variant, it will be destroyed when exiting
  // this scope.
  auto fidl_txn = FromDdkInternalTransaction(ddk::internal::Transaction::FromTxn(txn));
  std::visit([&](auto&& arg) { arg->Reply(&message); }, fidl_txn);
  return ZX_OK;
}

// Bitmask for checking if a pointer stashed in a ddk::internal::Transaction is from the heap or
// not. This is safe to use on our pointers, because fidl::Transactions are always aligned to more
// than one byte.
static_assert(alignof(fidl::Transaction) > 1);
constexpr uintptr_t kTransactionIsBoxed = 0x1;

}  // namespace

ddk::internal::Transaction MakeDdkInternalTransaction(fidl::Transaction* txn) {
  device_fidl_txn_t fidl_txn = {};
  fidl_txn.txn = {
      .reply = DdkReply,
  };
  fidl_txn.driver_host_context = reinterpret_cast<uintptr_t>(txn);
  return ddk::internal::Transaction(fidl_txn);
}

ddk::internal::Transaction MakeDdkInternalTransaction(std::unique_ptr<fidl::Transaction> txn) {
  device_fidl_txn_t fidl_txn = {};
  fidl_txn.txn = {
      .reply = DdkReply,
  };
  fidl_txn.driver_host_context = reinterpret_cast<uintptr_t>(txn.release()) | kTransactionIsBoxed;
  return ddk::internal::Transaction(fidl_txn);
}

std::variant<fidl::Transaction*, std::unique_ptr<fidl::Transaction>> FromDdkInternalTransaction(
    ddk::internal::Transaction* txn) {
  uintptr_t raw = txn->DriverHostCtx();
  ZX_ASSERT_MSG(raw != 0, "Reused a fidl_txn_t!\n");

  // Invalidate the source transaction
  txn->DeviceFidlTxn()->driver_host_context = 0;

  auto ptr = reinterpret_cast<fidl::Transaction*>(raw & ~kTransactionIsBoxed);
  if (raw & kTransactionIsBoxed) {
    return std::unique_ptr<fidl::Transaction>(ptr);
  }
  return ptr;
}
