// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "request_list.h"

#include <lib/ddk/debug.h>

namespace ufs {

zx::result<RequestList> RequestList::Create(zx::unowned_bti bti, size_t entry_size,
                                            uint8_t entry_count) {
  RequestList list;
  if (zx::result<> result = list.Init(std::move(bti), entry_size, entry_count); result.is_error()) {
    return result.take_error();
  }
  return zx::ok(std::move(list));
}

zx::result<> RequestList::Init(zx::unowned_bti bti, size_t entry_size, uint8_t entry_count) {
  ZX_ASSERT_MSG(entry_count > 0 && entry_count <= kMaxRequestListSize, "Invalid entry_count");
  size_t list_size = entry_count * entry_size;
  // Request list size should be less than page size (4KiB).
  // - Transfer request list size = Transfer request descriptor (32B) * 32 slot = 1KiB
  // - Task management request list size = Task management request descriptor (80B) * 8 slot = 640B
  ZX_ASSERT_MSG(list_size <= zx_system_get_page_size(), "Invalid list_size");

  // Allocate list.
  zx::result<> result = IoBufferInit(bti, &io_buffer_, list_size);
  if (result.is_error()) {
    FDF_LOG(ERROR, "Failed to allocate memory for the Request List: %s", result.status_string());
    return result.take_error();
  }
  request_slots_.resize(entry_count);

  // Allocate slots.
  for (auto &slot : request_slots_) {
    slot.state = SlotState::kFree;
    zx::result<> result = IoBufferInit(bti, &slot.command_descriptor_io, kUtpCommandDescriptorSize);
    if (result.is_error()) {
      FDF_LOG(ERROR, "Failed to allocate memory for the Command Descriptor: %s",
              result.status_string());
      return result.take_error();
    }
  }

  return zx::ok();
}

zx::result<> RequestList::IoBufferInit(zx::unowned_bti &bti,
                                       std::unique_ptr<dma_buffer::ContiguousBuffer> *io,
                                       size_t size) {
  const size_t buffer_size = fbl::round_up(size, zx_system_get_page_size());
  auto buffer_factory = dma_buffer::CreateBufferFactory();
  if (zx_status_t status = buffer_factory->CreateContiguous(*bti, buffer_size, 0, true, io);
      status != ZX_OK) {
    return zx::error(status);
  }
  if (!io->get()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  return zx::ok();
}

}  // namespace ufs
