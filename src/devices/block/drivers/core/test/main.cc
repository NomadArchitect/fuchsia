// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/hardware/block/c/banjo.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fit/defer.h>
#include <string.h>
#include <unistd.h>

#include <zxtest/zxtest.h>

#include "block-device.h"
#include "server.h"
#include "src/devices/testing/mock-ddk/mock-device.h"
#include "test/stub-block-device.h"

namespace {

TEST(ServerTest, Create) {
  StubBlockDevice blkdev;
  ddk::BlockProtocolClient client(blkdev.proto());
  zx::result server = Server::Create(&client);
  ASSERT_OK(server);
  std::thread thread([&server = server.value()]() { server->Serve(); });
  auto join = fit::defer([&server = server.value(), &thread]() {
    server->Close();
    thread.join();
  });
}

TEST(ServerTest, AttachVmo) {
  StubBlockDevice blkdev;
  ddk::BlockProtocolClient client(blkdev.proto());
  zx::result server = Server::Create(&client);
  ASSERT_OK(server);
  std::thread thread([&server = server.value()]() { server->Serve(); });
  auto join = fit::defer([&server = server.value(), &thread]() {
    server->Close();
    thread.join();
  });

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(8192, 0, &vmo));

  zx::result vmoid = server.value()->AttachVmo(std::move(vmo));
  ASSERT_OK(vmoid);
}

TEST(ServerTest, CloseVMO) {
  StubBlockDevice blkdev;
  ddk::BlockProtocolClient client(blkdev.proto());
  zx::result server = Server::Create(&client);
  ASSERT_OK(server);
  std::thread thread([&server = server.value()]() { server->Serve(); });
  auto join = fit::defer([&server = server.value(), &thread]() {
    server->Close();
    thread.join();
  });
  zx::result fifo = server.value()->GetFifo();
  ASSERT_OK(fifo);
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(8192, 0, &vmo));
  zx::result vmoid = server.value()->AttachVmo(std::move(vmo));
  ASSERT_OK(vmoid);

  // Request close VMO.
  block_fifo_request_t req = {
      .opcode = BLOCK_OP_CLOSE_VMO,
      .reqid = 0x100,
      .group = 0,
      .vmoid = vmoid.value(),
      .length = 0,
      .vmo_offset = 0,
      .dev_offset = 0,
  };

  // Write request.
  size_t actual_count = 0;
  ASSERT_OK(fifo.value().write(sizeof(req), &req, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);

  // Wait for response.
  zx_signals_t observed;
  ASSERT_OK(fifo.value().wait_one(ZX_FIFO_READABLE, zx::time::infinite(), &observed));

  block_fifo_response_t res;
  ASSERT_OK(fifo.value().read(sizeof(res), &res, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);
  ASSERT_OK(res.status);
  ASSERT_EQ(req.reqid, res.reqid);
  ASSERT_EQ(res.count, 1);
}

zx_status_t FillVMO(const zx::vmo& vmo, size_t size) {
  std::vector<uint8_t> buf(zx_system_get_page_size());
  memset(buf.data(), 0x44, zx_system_get_page_size());
  for (size_t i = 0; i < size; i += zx_system_get_page_size()) {
    size_t remain = size - i;
    if (remain > zx_system_get_page_size()) {
      remain = zx_system_get_page_size();
    }
    if (zx_status_t status = vmo.write(buf.data(), i, remain); status != ZX_OK) {
      return status;
    }
  }
  return ZX_OK;
}

TEST(ServerTest, ReadSingleTest) {
  StubBlockDevice blkdev;
  ddk::BlockProtocolClient client(blkdev.proto());
  zx::result server = Server::Create(&client);
  ASSERT_OK(server);
  std::thread thread([&server = server.value()]() { server->Serve(); });
  auto join = fit::defer([&server = server.value(), &thread]() {
    server->Close();
    thread.join();
  });
  zx::result fifo = server.value()->GetFifo();
  ASSERT_OK(fifo);

  const size_t vmo_size = 8192;
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(vmo_size, 0, &vmo));
  ASSERT_OK(FillVMO(vmo, vmo_size));

  zx::result vmoid = server.value()->AttachVmo(std::move(vmo));
  ASSERT_OK(vmoid);

  // Request close VMO.
  block_fifo_request_t req = {
      .opcode = BLOCK_OP_READ,
      .reqid = 0x100,
      .group = 0,
      .vmoid = vmoid.value(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };

  // Write request.
  size_t actual_count = 0;
  ASSERT_OK(fifo.value().write(sizeof(req), &req, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);

  // Wait for response.
  zx_signals_t observed;
  ASSERT_OK(zx_object_wait_one(fifo.value().get(), ZX_FIFO_READABLE, ZX_TIME_INFINITE, &observed));

  block_fifo_response_t res;
  ASSERT_OK(fifo.value().read(sizeof(res), &res, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);
  ASSERT_OK(res.status);
  ASSERT_EQ(req.reqid, res.reqid);
  ASSERT_EQ(res.count, 1);
}

TEST(ServerTest, ReadManyBlocksHasOneResponse) {
  StubBlockDevice blkdev;
  // Restrict max_transfer_size so that the server has to split up our request.
  block_info_t block_info = {
      .block_count = kBlockCount, .block_size = kBlockSize, .max_transfer_size = kBlockSize};
  blkdev.SetInfo(&block_info);
  ddk::BlockProtocolClient client(blkdev.proto());
  zx::result server = Server::Create(&client);
  ASSERT_OK(server);
  std::thread thread([&server = server.value()]() { server->Serve(); });
  auto join = fit::defer([&server = server.value(), &thread]() {
    server->Close();
    thread.join();
  });

  zx::result fifo = server.value()->GetFifo();
  ASSERT_OK(fifo);

  const size_t vmo_size = 8192;
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(vmo_size, 0, &vmo));
  ASSERT_OK(FillVMO(vmo, vmo_size));

  zx::result vmoid = server.value()->AttachVmo(std::move(vmo));
  ASSERT_OK(vmoid);

  block_fifo_request_t reqs[2] = {
      {
          .opcode = BLOCK_OP_READ,
          .reqid = 0x100,
          .group = 0,
          .vmoid = vmoid.value(),
          .length = 4,
          .vmo_offset = 0,
          .dev_offset = 0,
      },
      {
          .opcode = BLOCK_OP_READ,
          .reqid = 0x101,
          .group = 0,
          .vmoid = vmoid.value(),
          .length = 1,
          .vmo_offset = 0,
          .dev_offset = 0,
      },
  };

  // Write requests.
  size_t actual_count = 0;
  ASSERT_OK(fifo.value().write(sizeof(reqs[0]), reqs, 2, &actual_count));
  ASSERT_EQ(actual_count, 2);

  // Wait for first response.
  zx_signals_t observed;
  ASSERT_OK(zx_object_wait_one(fifo.value().get(), ZX_FIFO_READABLE, ZX_TIME_INFINITE, &observed));

  block_fifo_response_t res;
  ASSERT_OK(fifo.value().read(sizeof(res), &res, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);
  ASSERT_OK(res.status);
  ASSERT_EQ(reqs[0].reqid, res.reqid);
  ASSERT_EQ(res.count, 1);

  // Wait for second response.
  ASSERT_OK(zx_object_wait_one(fifo.value().get(), ZX_FIFO_READABLE, ZX_TIME_INFINITE, &observed));

  ASSERT_OK(fifo.value().read(sizeof(res), &res, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);
  ASSERT_OK(res.status);
  ASSERT_EQ(reqs[1].reqid, res.reqid);
  ASSERT_EQ(res.count, 1);
}

TEST(ServerTest, TestLargeGroupedTransaction) {
  StubBlockDevice blkdev;
  // Restrict max_transfer_size so that the server has to split up our request.
  block_info_t block_info = {
      .block_count = kBlockCount, .block_size = kBlockSize, .max_transfer_size = kBlockSize};
  blkdev.SetInfo(&block_info);
  ddk::BlockProtocolClient client(blkdev.proto());
  zx::result server = Server::Create(&client);
  ASSERT_OK(server);
  std::thread thread([&server = server.value()]() { server->Serve(); });
  auto join = fit::defer([&server = server.value(), &thread]() {
    server->Close();
    thread.join();
  });
  zx::result fifo = server.value()->GetFifo();
  ASSERT_OK(fifo);

  const size_t vmo_size = 8192;
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(vmo_size, 0, &vmo));
  ASSERT_OK(FillVMO(vmo, vmo_size));

  zx::result vmoid = server.value()->AttachVmo(std::move(vmo));
  ASSERT_OK(vmoid);

  block_fifo_request_t reqs[2] = {
      {
          .opcode = BLOCK_OP_READ | BLOCK_GROUP_ITEM,
          .reqid = 0x101,
          .group = 0,
          .vmoid = vmoid.value(),
          .length = 4,
          .vmo_offset = 0,
          .dev_offset = 0,
      },
      {
          .opcode = BLOCK_OP_READ | BLOCK_GROUP_ITEM | BLOCK_GROUP_LAST,
          .reqid = 0x101,
          .group = 0,
          .vmoid = vmoid.value(),
          .length = 1,
          .vmo_offset = 0,
          .dev_offset = 0,
      },
  };

  // Write requests.
  size_t actual_count = 0;
  ASSERT_OK(fifo.value().write(sizeof(reqs[0]), reqs, 2, &actual_count));
  ASSERT_EQ(actual_count, 2);

  // Wait for first response.
  zx_signals_t observed;
  ASSERT_OK(zx_object_wait_one(fifo.value().get(), ZX_FIFO_READABLE, ZX_TIME_INFINITE, &observed));

  block_fifo_response_t res;
  ASSERT_OK(fifo.value().read(sizeof(res), &res, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);
  ASSERT_OK(res.status);
  ASSERT_EQ(reqs[0].reqid, res.reqid);
  ASSERT_EQ(res.count, 2);
  ASSERT_EQ(res.group, 0);
}

TEST(BlockTest, TestReadWriteSingle) {
  auto fake_parent = MockDevice::FakeRootParent();
  StubBlockDevice blkdev;
  fake_parent->AddProtocol(ZX_PROTOCOL_BLOCK_IMPL, blkdev.proto()->ops, &blkdev);
  ASSERT_OK(BlockDevice::Bind(nullptr, fake_parent.get()));
  MockDevice* child_dev = fake_parent->GetLatestChild();
  auto* dut = child_dev->GetDeviceContext<BlockDevice>();

  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);

  zx::result endpoints = fidl::CreateEndpoints<fuchsia_hardware_block_volume::Volume>();
  ASSERT_OK(endpoints.status_value());
  auto& [client_end, server_end] = endpoints.value();

  fidl::BindServer(loop.dispatcher(), std::move(server_end), dut);

  zx::vmo vmo;
  constexpr uint64_t kBufferLength = kBlockCount * 5;
  ASSERT_OK(zx::vmo::create(kBufferLength, 0, &vmo));

  fidl::WireClient<fuchsia_hardware_block_volume::Volume> client(std::move(client_end),
                                                                 loop.dispatcher());
  client->WriteBlocks(std::move(vmo), kBufferLength, 0, 0)
      .ThenExactlyOnce(
          [](fidl::WireUnownedResult<fuchsia_hardware_block_volume::Volume::WriteBlocks>& result) {
            ASSERT_OK(result.status());
            const fit::result response = result.value();
            ASSERT_TRUE(response.is_ok(), "%s", zx_status_get_string(response.error_value()));
          });
  ASSERT_OK(loop.RunUntilIdle());
}

}  // namespace
