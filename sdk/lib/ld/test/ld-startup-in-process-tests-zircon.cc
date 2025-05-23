// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ld-startup-in-process-tests-zircon.h"

#include <dlfcn.h>
#include <lib/ld/abi.h>
#include <lib/zx/channel.h>
#include <zircon/syscalls.h>

#include <cstddef>
#include <string>

#include <gtest/gtest.h>

namespace ld::testing {
namespace {

// The dynamic linker gets loaded into this same test process, but it's given
// a sub-VMAR to consider its "root" or allocation range so hopefully it will
// confine its pointer references to that part of the address space.  The
// dynamic linker doesn't necessarily clean up all its mappings--on success,
// it leaves many mappings in place.  Test VMAR is always destroyed when the
// InProcessTestLaunch object goes out of scope.
constexpr size_t kVmarSize = 1 << 30;

void* GetVdso() {
  static void* vdso = [] {
    Dl_info info;
    EXPECT_TRUE(dladdr(reinterpret_cast<void*>(&_zx_process_exit), &info));
    EXPECT_STREQ(info.dli_fname, "<vDSO>");
    return info.dli_fbase;
  }();
  return vdso;
}

}  // namespace

void LdStartupInProcessTests::Init(std::initializer_list<std::string_view> args,
                                   std::initializer_list<std::string_view> env) {
  zx_vaddr_t test_base;
  ASSERT_EQ(zx::vmar::root_self()->allocate(
                ZX_VM_CAN_MAP_READ | ZX_VM_CAN_MAP_WRITE | ZX_VM_CAN_MAP_EXECUTE, 0, kVmarSize,
                &test_vmar_, &test_base),
            ZX_OK);

  fbl::unique_fd log_fd;
  ASSERT_NO_FATAL_FAILURE(InitLog(log_fd));
  ASSERT_NO_FATAL_FAILURE(bootstrap()  //
                              .AddInProcessTestHandles()
                              .AddAllocationVmar(test_vmar_.borrow())
                              .AddFd(STDERR_FILENO, std::move(log_fd))
                              .SetArgs(args)
                              .SetEnv(env));
}

void LdStartupInProcessTests::Load(std::string_view raw_executable_name,
                                   std::optional<std::string_view> expected_config) {
  const std::string executable_name =
      std::string(raw_executable_name) + std::string(kTestExecutableInProcessSuffix);

  ASSERT_TRUE(test_vmar_);  // Init must have been called already.

  // This points GetLibVmo() to the right place.
  LdsvcPathPrefix(executable_name);

  // This will adjust GetLibVmo() to use the libprefix from PT_INTERP so the
  // fetch of abi::kInterp will get the right install location.
  zx::vmo executable_vmo = GetExecutableVmoWithInterpConfig(executable_name, expected_config);
  ASSERT_TRUE(executable_vmo);

  // Prime the mock loader service from the Needed() calls.
  ASSERT_NO_FATAL_FAILURE(LdsvcExpectNeeded());

  std::optional<LoadResult> result;
  ASSERT_NO_FATAL_FAILURE(Load(GetInterp(executable_name, expected_config), result, test_vmar_));

  entry_ = result->entry + result->loader.load_bias();

  // The ends the useful lifetime of the loader object by extracting the VMAR
  // where it loaded the test image.  This VMAR handle doesn't need to be
  // saved here, since it's a sub-VMAR of the test_vmar_ that will be
  // destroyed when this InProcessTestLaunch object dies.
  zx::vmar load_image_vmar = std::move(result->loader).Commit(kNoRelro).TakeVmar();

  // Pass along that handle in the bootstrap message.
  ASSERT_NO_FATAL_FAILURE(procargs_.AddSelfVmar(std::move(load_image_vmar)));

  // Send the executable VMO.
  ASSERT_NO_FATAL_FAILURE(bootstrap().AddExecutableVmo(std::move(executable_vmo)));

  // If a mock loader service has been set up by calls to Needed() et al, send
  // the client end over.
  if (zx::channel ldsvc = TakeLdsvc()) {
    ASSERT_NO_FATAL_FAILURE(bootstrap().AddLdsvc(std::move(ldsvc)));
  }
}

int64_t LdStartupInProcessTests::Run() {
  using EntryFunction = int64_t(zx_handle_t, void*);
  auto fn = reinterpret_cast<EntryFunction*>(entry_);
  zx::channel bootstrap_receiver = bootstrap().PackBootstrap();
  return fn(bootstrap_receiver.release(), GetVdso());
}

LdStartupInProcessTests::~LdStartupInProcessTests() {
  if (test_vmar_) {
    EXPECT_EQ(test_vmar_.destroy(), ZX_OK);
  }
}

}  // namespace ld::testing
