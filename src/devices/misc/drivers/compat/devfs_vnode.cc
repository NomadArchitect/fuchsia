// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "devfs_vnode.h"

#include <lib/ddk/device.h>

#include <string_view>

#include <fbl/string_buffer.h>

#include "device.h"
#include "fbl/ref_ptr.h"
#include "src/devices/bin/driver_host/simple_binding.h"
#include "src/devices/lib/fidl/transaction.h"
#include "src/lib/storage/vfs/cpp/vfs_types.h"

namespace {

// Utility class for dispatching messages to a device.
class FidlDispatcher : public fidl::internal::IncomingMessageDispatcher {
 public:
  explicit FidlDispatcher(fbl::RefPtr<DevfsVnode> node) : node_(std::move(node)) {}

  static void CreateAndBind(fbl::RefPtr<DevfsVnode> node, async_dispatcher_t* dispatcher,
                            zx::channel channel);

 private:
  void dispatch_message(fidl::IncomingHeaderAndMessage&& msg, ::fidl::Transaction* txn,
                        fidl::internal::MessageStorageViewBase* storage_view) final;
  fbl::RefPtr<DevfsVnode> node_;
};

void FidlDispatcher::CreateAndBind(fbl::RefPtr<DevfsVnode> node, async_dispatcher_t* dispatcher,
                                   zx::channel channel) {
  auto fidl = std::make_unique<FidlDispatcher>(std::move(node));
  auto fidl_ptr = fidl.get();

  // Create the binding. We pass the FidlDispatcher's pointer into the unbound
  // function so it stays alive as long as the binding.
  auto binding = std::make_unique<devfs::SimpleBinding>(dispatcher, std::move(channel), fidl_ptr,
                                                        [fidl = std::move(fidl)](void*) {});
  devfs::BeginWait(&binding);
}

void FidlDispatcher::dispatch_message(fidl::IncomingHeaderAndMessage&& msg,
                                      ::fidl::Transaction* txn,
                                      fidl::internal::MessageStorageViewBase* storage_view) {
  // If the device is unbound it shouldn't receive messages so close the channel.
  if (!node_->dev()) {
    txn->Close(ZX_ERR_IO_NOT_PRESENT);
    return;
  }

  fidl_incoming_msg_t c_msg = std::move(msg).ReleaseToEncodedCMessage();
  auto ddk_txn = MakeDdkInternalTransaction(txn);
  zx_status_t status = node_->dev()->MessageOp(&c_msg, ddk_txn.Txn());
  if (status != ZX_OK && status != ZX_ERR_ASYNC) {
    // Close the connection on any error
    txn->Close(status);
  }
}

}  // namespace

zx_status_t DevfsVnode::OpenNode(fs::Vnode::ValidatedOptions options,
                                 fbl::RefPtr<Vnode>* out_redirect) {
  zx_device_t* dev_out = nullptr;
  auto status = dev_->OpenOp(&dev_out, static_cast<uint32_t>(options->ToIoV1Flags()));
  if (status != ZX_OK) {
    return status;
  }

  if (dev_out != nullptr && dev_out != dev_) {
    *out_redirect = dev_out->dev_vnode();
  }
  return ZX_OK;
}

zx_status_t DevfsVnode::CloseNode() {
  return dev_->CloseOp(0);
  ;
}

zx_status_t DevfsVnode::GetAttributes(fs::VnodeAttributes* a) {
  a->mode = V_TYPE_CDEV | V_IRUSR | V_IWUSR;
  a->content_size = 0;
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

void DevfsVnode::HandleFsSpecificMessage(fidl::IncomingHeaderAndMessage& msg,
                                         fidl::Transaction* txn) {
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

void DevfsVnode::ConnectToDeviceFidl(ConnectToDeviceFidlRequestView request,
                                     ConnectToDeviceFidlCompleter::Sync& completer) {
  FidlDispatcher::CreateAndBind(fbl::RefPtr(this), dev_->dispatcher(), std::move(request->server));
}

void DevfsVnode::Bind(BindRequestView request, BindCompleter::Sync& completer) {
  if (dev_->HasChildren()) {
    // A DFv1 driver will add a child device once it's bound. If the device has any children, refuse
    // the Bind() call.
    completer.ReplyError(ZX_ERR_ALREADY_BOUND);
    return;
  }
  auto async = completer.ToAsync();
  auto promise =
      dev_->RebindToLibname(std::string_view{request->driver.data(), request->driver.size()})
          .then(
              [completer = std::move(async)](fpromise::result<void, zx_status_t>& result) mutable {
                if (result.is_ok()) {
                  completer.ReplySuccess();
                } else {
                  completer.ReplyError(result.take_error());
                }
              });

  dev_->executor().schedule_task(std::move(promise));
}

void DevfsVnode::GetCurrentPerformanceState(GetCurrentPerformanceStateCompleter::Sync& completer) {
  completer.Reply(0);
}

void DevfsVnode::Rebind(RebindRequestView request, RebindCompleter::Sync& completer) {
  auto async = completer.ToAsync();
  auto promise =
      dev_->RebindToLibname(std::string_view{request->driver.data(), request->driver.size()})
          .then(
              [completer = std::move(async)](fpromise::result<void, zx_status_t>& result) mutable {
                if (result.is_ok()) {
                  completer.ReplySuccess();
                } else {
                  completer.ReplyError(result.take_error());
                }
              });

  dev_->executor().schedule_task(std::move(promise));
}

void DevfsVnode::UnbindChildren(UnbindChildrenCompleter::Sync& completer) {
  auto async = completer.ToAsync();
  dev_->executor().schedule_task(dev_->RemoveChildren().then(
      [completer = std::move(async)](fpromise::result<>& result) mutable {
        completer.ReplySuccess();
      }));
}

void DevfsVnode::ScheduleUnbind(ScheduleUnbindCompleter::Sync& completer) {
  dev_->Remove();
  completer.ReplySuccess();
}

void DevfsVnode::GetTopologicalPath(GetTopologicalPathCompleter::Sync& completer) {
  std::string path("/dev/");
  path.append(dev_->topological_path());
  completer.ReplySuccess(fidl::StringView::FromExternal(path));
}

void DevfsVnode::GetMinDriverLogSeverity(GetMinDriverLogSeverityCompleter::Sync& completer) {
  uint8_t severity = dev_->logger().GetSeverity();
  completer.Reply(ZX_OK, fuchsia_logger::wire::LogLevelFilter(severity));
}

void DevfsVnode::SetMinDriverLogSeverity(SetMinDriverLogSeverityRequestView request,
                                         SetMinDriverLogSeverityCompleter::Sync& completer) {
  FuchsiaLogSeverity severity = static_cast<FuchsiaLogSeverity>(request->severity);
  dev_->logger().SetSeverity(severity);
  completer.Reply(ZX_OK);
}

void DevfsVnode::SetPerformanceState(SetPerformanceStateRequestView request,
                                     SetPerformanceStateCompleter::Sync& completer) {
  auto result = dev_->SetPerformanceStateOp(request->requested_state);
  zx_status_t status = result.is_ok() ? ZX_OK : result.error_value();
  uint32_t out_state = result.is_ok() ? result.value() : 0;
  completer.Reply(status, out_state);
}
