// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/logger/cpp/fidl.h>
#include <fuchsia/validate/logs/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/sys/cpp/component_context.h>

#include "lib/async/dispatcher.h"
#include "lib/fdio/directory.h"
#include "lib/syslog/structured_backend/cpp/fuchsia_syslog.h"

zx_koid_t GetKoid(zx_handle_t handle) {
  zx_info_handle_basic_t info;
  zx_status_t status =
      zx_object_get_info(handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  return status == ZX_OK ? info.koid : ZX_KOID_INVALID;
}

class Puppet : public fuchsia::validate::logs::LogSinkPuppet {
 public:
  explicit Puppet(std::unique_ptr<sys::ComponentContext> context) : context_(std::move(context)) {
    context_->outgoing()->AddPublicService(sink_bindings_.GetHandler(this));
    ConnectAsync(true);
  }

  void HandleInterest() {
    log_sink_->WaitForInterestChange(
        [=](fuchsia::logger::LogSink_WaitForInterestChange_Result interest_result) {
          auto interest = std::move(interest_result.response().data);
          if (!interest.has_min_severity()) {
            min_log_level_ = FUCHSIA_LOG_INFO;
          } else {
            min_log_level_ = IntoLogSeverity(interest.min_severity());
          }
          fuchsia_syslog::LogBuffer buffer;

          BeginRecord(&buffer, min_log_level_, __FILE__, __LINE__, "Changed severity");
          buffer.FlushRecord();
          HandleInterest();
        });
  }

  void EmitPuppetStarted() {
    fuchsia_syslog::LogBuffer buffer;

    BeginRecord(&buffer, FUCHSIA_LOG_INFO, __FILE__, __LINE__, "Puppet started.");
    buffer.FlushRecord();
  }

  void BeginRecord(fuchsia_syslog::LogBuffer* buffer, FuchsiaLogSeverity severity,
                   cpp17::optional<cpp17::string_view> file_name, unsigned int line,
                   cpp17::optional<cpp17::string_view> msg) {
    buffer->BeginRecord(severity, file_name, line, msg, socket_.borrow(), 0,
                        GetKoid(zx_process_self()), GetKoid(zx_thread_self()));
  }

  static FuchsiaLogSeverity IntoLogSeverity(fuchsia::diagnostics::types::Severity severity) {
    switch (severity) {
      case fuchsia::diagnostics::types::Severity::TRACE:
        return FUCHSIA_LOG_TRACE;
      case fuchsia::diagnostics::types::Severity::DEBUG:
        return FUCHSIA_LOG_DEBUG;
      case fuchsia::diagnostics::types::Severity::INFO:
        return FUCHSIA_LOG_INFO;
      case fuchsia::diagnostics::types::Severity::WARN:
        return FUCHSIA_LOG_WARNING;
      case fuchsia::diagnostics::types::Severity::ERROR:
        return FUCHSIA_LOG_ERROR;
      case fuchsia::diagnostics::types::Severity::FATAL:
        return FUCHSIA_LOG_FATAL;
      default:
        return FUCHSIA_LOG_FATAL;
    }
  }

  void ConnectAsync(bool wait_for_initial_interest) {
    zx::channel logger, logger_request;
    if (zx::channel::create(0, &logger, &logger_request) != ZX_OK) {
      return;
    }
    // TODO(https://fxbug.dev/42154983): Support for custom names.
    if (fdio_service_connect("/svc/fuchsia.logger.LogSink", logger_request.release()) != ZX_OK) {
      return;
    }

    if (wait_for_initial_interest) {
      fuchsia::logger::LogSinkSyncPtr sync_log_sink;
      sync_log_sink.Bind(std::move(logger));
      fuchsia::logger::LogSink_WaitForInterestChange_Result interest_result;
      sync_log_sink->WaitForInterestChange(&interest_result);
      auto interest = std::move(interest_result.response().data);
      if (interest.has_min_severity()) {
        min_log_level_ = IntoLogSeverity(interest.min_severity());
      }
      logger = sync_log_sink.Unbind().TakeChannel();
    }

    if (log_sink_.Bind(std::move(logger)) != ZX_OK) {
      return;
    }
    zx::socket local, remote;
    if (zx::socket::create(ZX_SOCKET_DATAGRAM, &local, &remote) != ZX_OK) {
      return;
    }
    log_sink_->ConnectStructured(std::move(remote));
    socket_ = std::move(local);
    HandleInterest();
  }

  void StopInterestListener(StopInterestListenerCallback callback) override {
    min_log_level_ = FUCHSIA_LOG_TRACE;
    // Reconnect per C++ behavior.
    ConnectAsync(false);
    callback();
  }

  void GetInfo(GetInfoCallback callback) override {
    fuchsia::validate::logs::PuppetInfo info;
    info.pid = GetKoid(zx_process_self());
    info.tid = GetKoid(zx_thread_self());
    callback(info);
  }

  void EmitLog(fuchsia::validate::logs::RecordSpec spec, EmitLogCallback callback) override {
    fuchsia_syslog::LogBuffer buffer;
    BeginRecord(&buffer, static_cast<uint8_t>(spec.record.severity), spec.file.data(), spec.line,
                std::nullopt /* message */);
    for (auto& arg : spec.record.arguments) {
      switch (arg.value.Which()) {
        case fuchsia::validate::logs::Value::kUnknown:
        case fuchsia::validate::logs::Value::Invalid:
          break;
        case fuchsia::validate::logs::Value::kFloating:
          buffer.WriteKeyValue(arg.name.data(), arg.value.floating());
          break;
        case fuchsia::validate::logs::Value::kSignedInt:
          buffer.WriteKeyValue(arg.name.data(), arg.value.signed_int());
          break;
        case fuchsia::validate::logs::Value::kUnsignedInt:
          buffer.WriteKeyValue(arg.name.data(), arg.value.unsigned_int());
          break;
        case fuchsia::validate::logs::Value::kText:
          buffer.WriteKeyValue(arg.name.data(), arg.value.text().data());
          break;
        case fuchsia::validate::logs::Value::kBoolean:
          buffer.WriteKeyValue(arg.name.data(), arg.value.boolean());
          break;
      }
    }
    if (static_cast<uint8_t>(spec.record.severity) >= min_log_level_) {
      buffer.FlushRecord();
    }
    callback();
  }

 private:
  FuchsiaLogSeverity min_log_level_ = FUCHSIA_LOG_INFO;
  zx::socket socket_;
  fuchsia::logger::LogSinkPtr log_sink_;
  std::unique_ptr<sys::ComponentContext> context_;
  fidl::BindingSet<fuchsia::validate::logs::LogSinkPuppet> sink_bindings_;
};

int main(int argc, const char** argv) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  Puppet puppet(sys::ComponentContext::CreateAndServeOutgoingDirectory());
  puppet.EmitPuppetStarted();
  loop.Run();
}
