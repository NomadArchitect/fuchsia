// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdio/vfs.h>
#include <lib/sync/completion.h>
#include <lib/zx/vmo.h>

#include <memory>

#include <fbl/auto_lock.h>
#include <gtest/gtest.h>

#include "src/storage/lib/vfs/cpp/managed_vfs.h"
#include "src/storage/lib/vfs/cpp/vfs_types.h"
#include "zircon/status.h"

namespace {

namespace fio = fuchsia_io;

class TestVNode : public fs::Vnode {
 public:
  fuchsia_io::NodeProtocolKinds GetProtocols() const final {
    return fuchsia_io::NodeProtocolKinds::kFile;
  }
  zx_status_t GetVmo(fuchsia_io::wire::VmoFlags flags, zx::vmo* out_vmo) override {
    zx::vmo vmo;
    zx_status_t status = zx::vmo::create(4096, 0u, &vmo);
    EXPECT_EQ(status, ZX_OK);
    if (status != ZX_OK)
      return status;
    *out_vmo = std::move(vmo);
    return ZX_OK;
  }
};

TEST(ConnectionRightsTest, GetBackingMemoryWithServeDeprecated) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_EQ(loop.StartThread(), ZX_OK);

  std::unique_ptr<fs::ManagedVfs> vfs = std::make_unique<fs::ManagedVfs>(loop.dispatcher());

  using test_row_t = struct {
    fio::wire::OpenFlags connection_flags;  // Or'd OPEN_RIGHT_* flags for this connection.
    fio::wire::VmoFlags request_flags;
    zx_status_t expected_result;  // What we expect FileGetBuffer to return.
  };

  test_row_t test_data[] = {
      // If the connection has all rights, then everything should work.
      {
          .connection_flags = fio::wire::OpenFlags::kRightReadable |
                              fio::wire::OpenFlags::kRightWritable |
                              fio::wire::OpenFlags::kRightExecutable,
          .request_flags = fio::wire::VmoFlags::kRead,
          .expected_result = ZX_OK,
      },
      {
          .connection_flags = fio::wire::OpenFlags::kRightReadable |
                              fio::wire::OpenFlags::kRightWritable |
                              fio::wire::OpenFlags::kRightExecutable,
          .request_flags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kWrite,
          .expected_result = ZX_OK,
      },
      {
          .connection_flags = fio::wire::OpenFlags::kRightReadable |
                              fio::wire::OpenFlags::kRightWritable |
                              fio::wire::OpenFlags::kRightExecutable,
          .request_flags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kExecute,
          .expected_result = ZX_OK,
      },
      // If the connection is missing the EXECUTABLE right, then requests with
      // fio::wire::VmoFlags::kExecute should fail.
      {
          .connection_flags =
              fio::wire::OpenFlags::kRightReadable | fio::wire::OpenFlags::kRightWritable,
          .request_flags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kExecute,
          .expected_result = ZX_ERR_ACCESS_DENIED,
      },

      // If the connection is missing the WRITABLE right, then requests with
      // fio::wire::VmoFlags::kWrite should fail.
      {
          .connection_flags =
              fio::wire::OpenFlags::kRightReadable | fio::wire::OpenFlags::kRightExecutable,
          .request_flags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kWrite,
          .expected_result = ZX_ERR_ACCESS_DENIED,
      },
  };

  {
    auto vnode = fbl::AdoptRef<TestVNode>(new TestVNode());
    for (test_row_t& row : test_data) {
      // Set up a vfs connection with the testcase's connection flags
      auto file = fidl::Endpoints<fio::File>::Create();
      zx::result options = fs::VnodeConnectionOptions::FromOpen1Flags(row.connection_flags);
      ASSERT_TRUE(options.is_ok());
      vfs->ServeDeprecated(vnode, file.server.TakeChannel(), *options);

      // Call FileGetBuffer on the channel with the testcase's request flags. Check that we get the
      // expected result.
      const fidl::WireResult result =
          fidl::WireCall(file.client)->GetBackingMemory(row.request_flags);
      EXPECT_TRUE(result.ok()) << result.FormatDescription();
      const auto& response = result.value();

      // Verify that the result matches the value in our test table.
      if (row.expected_result == ZX_OK) {
        EXPECT_TRUE(response.is_ok()) << zx_status_get_string(response.error_value());
      } else {
        EXPECT_TRUE(response.is_error());
        EXPECT_EQ(response.error_value(), row.expected_result);
      }
    }
  }

  // Tear down the VFS. On completion, it will no longer rely on the async loop. Then, tear down the
  // async loop.
  sync_completion_t completion;
  vfs->Shutdown([&completion](zx_status_t status) {
    EXPECT_EQ(status, ZX_OK);
    sync_completion_signal(&completion);
  });
  sync_completion_wait(&completion, zx::time::infinite().get());
  loop.Shutdown();
}

TEST(ConnectionRightsTest, GetBackingMemoryWithServe) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_EQ(loop.StartThread(), ZX_OK);

  std::unique_ptr<fs::ManagedVfs> vfs = std::make_unique<fs::ManagedVfs>(loop.dispatcher());

  struct TestCase {
    fio::Flags flags;
    fio::VmoFlags get_backing_memory_flags;
    zx_status_t get_backing_memory_result;  // What we expect FileGetBuffer to return.
  };

  TestCase test_data[] = {
      // If the connection has all rights, then everything should work.
      {
          .flags = fio::Flags::kPermRead | fio::Flags::kPermWrite | fio::Flags::kPermExecute,
          .get_backing_memory_flags = fio::wire::VmoFlags::kRead,
          .get_backing_memory_result = ZX_OK,
      },
      {
          .flags = fio::Flags::kPermRead | fio::Flags::kPermWrite | fio::Flags::kPermExecute,
          .get_backing_memory_flags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kWrite,
          .get_backing_memory_result = ZX_OK,
      },
      {
          .flags = fio::Flags::kPermRead | fio::Flags::kPermWrite | fio::Flags::kPermExecute,
          .get_backing_memory_flags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kExecute,
          .get_backing_memory_result = ZX_OK,
      },
      // If the connection is missing the EXECUTABLE right, then requests with
      // fio::wire::VmoFlags::kExecute should fail.
      {
          .flags = fio::Flags::kPermRead | fio::Flags::kPermWrite,
          .get_backing_memory_flags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kExecute,
          .get_backing_memory_result = ZX_ERR_ACCESS_DENIED,
      },
      // If the connection is missing the WRITABLE right, then requests with
      // fio::wire::VmoFlags::kWrite should fail.
      {
          .flags = fio::Flags::kPermRead | fio::Flags::kPermExecute,
          .get_backing_memory_flags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kWrite,
          .get_backing_memory_result = ZX_ERR_ACCESS_DENIED,
      },
  };

  {
    auto vnode = fbl::MakeRefCounted<TestVNode>();
    for (const auto& test_case : test_data) {
      // Set up a vfs connection with the testcase's connection flags
      auto file = fidl::Endpoints<fio::File>::Create();
      auto serve_result = vfs->Serve(vnode, file.server.TakeChannel(), test_case.flags);
      ASSERT_EQ(serve_result, ZX_OK) << zx_status_get_string(serve_result);
      // Call FileGetBuffer on the channel with the testcase's request flags. Check that we get the
      // expected result.
      const fidl::WireResult result =
          fidl::WireCall(file.client)->GetBackingMemory(test_case.get_backing_memory_flags);
      ASSERT_TRUE(result.ok()) << result.FormatDescription();
      const auto& response = result.value();

      // Verify that the result matches the value in our test table.
      if (test_case.get_backing_memory_result == ZX_OK) {
        EXPECT_TRUE(response.is_ok()) << zx_status_get_string(response.error_value());
      } else {
        EXPECT_TRUE(response.is_error());
        EXPECT_EQ(response.error_value(), test_case.get_backing_memory_result);
      }
    }
  }

  // Tear down the VFS. On completion, it will no longer rely on the async loop. Then, tear down the
  // async loop.
  sync_completion_t completion;
  vfs->Shutdown([&completion](zx_status_t status) {
    EXPECT_EQ(status, ZX_OK);
    sync_completion_signal(&completion);
  });
  sync_completion_wait(&completion, zx::time::infinite().get());
  loop.Shutdown();
}

}  // namespace
