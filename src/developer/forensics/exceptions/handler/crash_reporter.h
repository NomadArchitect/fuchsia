// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_EXCEPTIONS_HANDLER_CRASH_REPORTER_H_
#define SRC_DEVELOPER_FORENSICS_EXCEPTIONS_HANDLER_CRASH_REPORTER_H_

#include <fidl/fuchsia.driver.crash/cpp/fidl.h>
#include <fuchsia/exception/cpp/fidl.h>
#include <fuchsia/exception/internal/cpp/fidl.h>
#include <lib/async/cpp/executor.h>
#include <lib/fit/function.h>
#include <lib/fpromise/promise.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/zx/exception.h>
#include <lib/zx/time.h>
#include <zircon/types.h>

#include <memory>

#include "src/developer/forensics/exceptions/handler/wake_lease.h"

namespace forensics {
namespace exceptions {
namespace handler {

// Handles asynchronously building and filing a crash report for a given zx::exception.
class CrashReporter : public fuchsia::exception::internal::CrashReporter {
 public:
  CrashReporter(async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
                zx::duration component_lookup_timeout, std::unique_ptr<WakeLeaseBase> wake_lease,
                fidl::ClientEnd<fuchsia_driver_crash::CrashIntrospect> driver_crash_introspect);

  // |fuchsia::exception::internal::CrashReporter|
  virtual void Send(zx::exception exception, zx::process crashed_proces, zx::thread crashed_thread,
                    SendCallback callback) override;

 private:
  async_dispatcher_t* dispatcher_;
  async::Executor executor_;
  std::shared_ptr<sys::ServiceDirectory> services_;
  zx::duration component_lookup_timeout_;
  std::unique_ptr<WakeLeaseBase> wake_lease_;
  fidl::Client<fuchsia_driver_crash::CrashIntrospect> driver_crash_introspect_;
};

}  // namespace handler
}  // namespace exceptions
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_EXCEPTIONS_HANDLER_CRASH_REPORTER_H_
