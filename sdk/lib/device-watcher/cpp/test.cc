// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <zircon/time.h>

#include <memory>

#include <fbl/ref_ptr.h>
#include <fbl/unique_fd.h>
#include <zxtest/zxtest.h>

#include "lib/async-loop/loop.h"
#include "src/lib/storage/vfs/cpp/managed_vfs.h"
#include "src/lib/storage/vfs/cpp/pseudo_dir.h"
#include "src/lib/storage/vfs/cpp/pseudo_file.h"
#include "src/lib/storage/vfs/cpp/service.h"

TEST(DeviceWatcherTest, Smoke) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  auto file = fbl::MakeRefCounted<fs::UnbufferedPseudoFile>(
      [](fbl::String* output) { return ZX_OK; }, [](std::string_view input) { return ZX_OK; });

  auto third = fbl::MakeRefCounted<fs::PseudoDir>();
  third->AddEntry("file", file);

  auto second = fbl::MakeRefCounted<fs::PseudoDir>();
  second->AddEntry("third", std::move(third));

  auto first = fbl::MakeRefCounted<fs::PseudoDir>();
  first->AddEntry("second", std::move(second));
  first->AddEntry("file", file);

  auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
  ASSERT_EQ(ZX_OK, endpoints.status_value());

  loop.StartThread();
  fs::ManagedVfs vfs(loop.dispatcher());

  vfs.ServeDirectory(first, std::move(endpoints->server));

  fbl::unique_fd dir;
  ASSERT_EQ(ZX_OK,
            fdio_fd_create(endpoints->client.TakeChannel().release(), dir.reset_and_get_address()));

  ASSERT_EQ(ZX_OK, device_watcher::WaitForFile(dir.get(), "file").status_value());

  ASSERT_EQ(ZX_OK,
            device_watcher::RecursiveWaitForFile(dir.get(), "second/third/file").status_value());

  ASSERT_EQ(
      ZX_OK,
      device_watcher::RecursiveWaitForFileReadOnly(dir.get(), "second/third/file").status_value());

  sync_completion_t shutdown;

  vfs.Shutdown([&shutdown](zx_status_t status) {
    sync_completion_signal(&shutdown);
    ASSERT_EQ(status, ZX_OK);
  });
  ASSERT_EQ(sync_completion_wait(&shutdown, zx::duration::infinite().get()), ZX_OK);
}

TEST(DeviceWatcherTest, OpenInNamespace) {
  ASSERT_EQ(device_watcher::RecursiveWaitForFileReadOnly("/dev/sys/test").status_value(), ZX_OK);
  ASSERT_EQ(device_watcher::RecursiveWaitForFile("/dev/sys/test").status_value(), ZX_OK);

  ASSERT_EQ(device_watcher::RecursiveWaitForFile("/other-test/file").status_value(),
            ZX_ERR_NOT_SUPPORTED);
}

TEST(DeviceWatcherTest, DirWatcherWaitForRemoval) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  auto file = fbl::MakeRefCounted<fs::UnbufferedPseudoFile>(
      [](fbl::String* output) { return ZX_OK; }, [](std::string_view input) { return ZX_OK; });

  auto third = fbl::MakeRefCounted<fs::PseudoDir>();
  third->AddEntry("file", file);

  auto second = fbl::MakeRefCounted<fs::PseudoDir>();
  second->AddEntry("third", third);

  auto first = fbl::MakeRefCounted<fs::PseudoDir>();
  first->AddEntry("second", second);
  first->AddEntry("file", file);

  auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
  ASSERT_EQ(ZX_OK, endpoints.status_value());

  loop.StartThread();
  fs::ManagedVfs vfs(loop.dispatcher());

  vfs.ServeDirectory(first, std::move(endpoints->server));

  fbl::unique_fd dir;
  ASSERT_EQ(ZX_OK,
            fdio_fd_create(endpoints->client.TakeChannel().release(), dir.reset_and_get_address()));
  fbl::unique_fd sub_dir(openat(dir.get(), "second/third", O_DIRECTORY | O_RDONLY));

  ASSERT_EQ(ZX_OK, device_watcher::WaitForFile(dir.get(), "file").status_value());
  ASSERT_EQ(ZX_OK,
            device_watcher::RecursiveWaitForFile(dir.get(), "second/third/file").status_value());

  // Verify removal of the root directory file
  std::unique_ptr<device_watcher::DirWatcher> root_watcher;
  ASSERT_EQ(ZX_OK, device_watcher::DirWatcher::Create(dir.get(), &root_watcher));

  first->RemoveEntry("file");
  ASSERT_EQ(ZX_OK, root_watcher->WaitForRemoval("file", zx::duration::infinite()));

  // Verify removal of the subdirectory file
  std::unique_ptr<device_watcher::DirWatcher> sub_watcher;
  ASSERT_EQ(ZX_OK, device_watcher::DirWatcher::Create(sub_dir.get(), &sub_watcher));

  third->RemoveEntry("file");
  ASSERT_EQ(ZX_OK, sub_watcher->WaitForRemoval("file", zx::duration::infinite()));

  sync_completion_t shutdown;

  vfs.Shutdown([&shutdown](zx_status_t status) {
    sync_completion_signal(&shutdown);
    ASSERT_EQ(status, ZX_OK);
  });
  ASSERT_EQ(sync_completion_wait(&shutdown, zx::duration::infinite().get()), ZX_OK);
}

TEST(DeviceWatcherTest, DirWatcherVerifyUnowned) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  auto file = fbl::MakeRefCounted<fs::UnbufferedPseudoFile>(
      [](fbl::String* output) { return ZX_OK; }, [](std::string_view input) { return ZX_OK; });

  auto first = fbl::MakeRefCounted<fs::PseudoDir>();
  first->AddEntry("file", file);

  auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
  ASSERT_EQ(ZX_OK, endpoints.status_value());

  loop.StartThread();
  fs::ManagedVfs vfs(loop.dispatcher());

  vfs.ServeDirectory(first, std::move(endpoints->server));

  fbl::unique_fd dir;
  ASSERT_EQ(ZX_OK,
            fdio_fd_create(endpoints->client.TakeChannel().release(), dir.reset_and_get_address()));

  ASSERT_EQ(ZX_OK, device_watcher::WaitForFile(dir.get(), "file").status_value());

  std::unique_ptr<device_watcher::DirWatcher> root_watcher;
  ASSERT_EQ(ZX_OK, device_watcher::DirWatcher::Create(dir.get(), &root_watcher));

  // Close the directory fd
  ASSERT_EQ(ZX_OK, dir.reset());

  // Verify the watcher can still successfully wait for removal
  first->RemoveEntry("file");
  ASSERT_EQ(ZX_OK, root_watcher->WaitForRemoval("file", zx::duration::infinite()));

  sync_completion_t shutdown;

  vfs.Shutdown([&shutdown](zx_status_t status) {
    sync_completion_signal(&shutdown);
    ASSERT_EQ(status, ZX_OK);
  });
  ASSERT_EQ(sync_completion_wait(&shutdown, zx::duration::infinite().get()), ZX_OK);
}

class IterateDirectoryTest : public zxtest::Test {
 public:
  IterateDirectoryTest()
      : loop_(&kAsyncLoopConfigNoAttachToCurrentThread), vfs_(loop_.dispatcher()) {}

  void SetUp() override {
    // Set up the fake filesystem.
    auto file1 = fbl::MakeRefCounted<fs::UnbufferedPseudoFile>(
        [](fbl::String* output) { return ZX_OK; }, [](std::string_view input) { return ZX_OK; });
    auto file2 = fbl::MakeRefCounted<fs::UnbufferedPseudoFile>(
        [](fbl::String* output) { return ZX_OK; }, [](std::string_view input) { return ZX_OK; });

    auto first = fbl::MakeRefCounted<fs::PseudoDir>();
    first->AddEntry("file1", file1);
    first->AddEntry("file2", file2);

    auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
    ASSERT_EQ(ZX_OK, endpoints.status_value());

    loop_.StartThread();
    vfs_.ServeDirectory(first, std::move(endpoints->server));

    ASSERT_EQ(ZX_OK, fdio_fd_create(endpoints->client.TakeChannel().release(),
                                    dir_.reset_and_get_address()));
  }

  void TearDown() override {
    sync_completion_t shutdown;
    vfs_.Shutdown([&shutdown](zx_status_t status) {
      sync_completion_signal(&shutdown);
      ASSERT_EQ(status, ZX_OK);
    });
    ASSERT_EQ(ZX_OK, sync_completion_wait(&shutdown, zx::duration::infinite().get()));
    loop_.Shutdown();
  }

 protected:
  async::Loop loop_;
  fs::ManagedVfs vfs_;
  fbl::unique_fd dir_;
};

TEST_F(IterateDirectoryTest, IterateDirectory) {
  std::vector<std::string> seen;
  zx_status_t status = device_watcher::IterateDirectory(
      dir_.get(), [&seen](std::string_view filename, zx::channel channel) {
        // Collect the file names into the vector.
        seen.emplace_back(filename);
        return ZX_OK;
      });
  ASSERT_EQ(ZX_OK, status);

  // Make sure the file names seen were as expected.
  ASSERT_EQ(2, seen.size());
  std::sort(seen.begin(), seen.end());
  ASSERT_EQ("file1", seen[0]);
  ASSERT_EQ("file2", seen[1]);
}

TEST_F(IterateDirectoryTest, IterateDirectoryCancelled) {
  // Test that iteration is cancelled when the callback returns an error
  std::vector<std::string> seen;
  zx_status_t status = device_watcher::IterateDirectory(
      dir_.get(), [&seen](std::string_view filename, zx::channel channel) {
        seen.emplace_back(filename);
        return ZX_ERR_INTERNAL;
      });
  ASSERT_EQ(ZX_ERR_INTERNAL, status);

  // Should only have seen a single file before exiting.
  ASSERT_EQ(1, seen.size());
}

TEST_F(IterateDirectoryTest, IterateDirectoryChannel) {
  // Test that we can use the channel passed to the callback function to make
  // fuchsia.io.Node calls.
  std::vector<uint64_t> content_sizes;
  zx_status_t status = device_watcher::IterateDirectory(
      dir_.get(), [&content_sizes](std::string_view filename, zx::channel channel) {
        auto result =
            fidl::WireCall(fidl::UnownedClientEnd<fuchsia_io::Node>(channel.borrow()))->GetAttr();
        if (!result.ok()) {
          return result.status();
        }
        content_sizes.push_back(result.value().attributes.content_size);
        return ZX_OK;
      });
  ASSERT_EQ(ZX_OK, status);

  ASSERT_EQ(2, content_sizes.size());

  // Files are empty.
  ASSERT_EQ(0, content_sizes[0]);
  ASSERT_EQ(0, content_sizes[1]);
}
