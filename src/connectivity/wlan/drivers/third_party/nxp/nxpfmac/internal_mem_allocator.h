// Copyright (c) 2022 The Fuchsia Authors
//
// Permission to use, copy, modify, and/or distribute this software for any purpose with or without
// fee is hereby granted, provided that the above copyright notice and this permission notice
// appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
// SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE
// AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT,
// NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
// OF THIS SOFTWARE.

#ifndef SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_NXP_NXPFMAC_INTERNAL_MEM_ALLOCATOR_H_
#define SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_NXP_NXPFMAC_INTERNAL_MEM_ALLOCATOR_H_

#include <lib/zx/result.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <mutex>
#include <unordered_map>

#include <region-alloc/region-alloc.h>

#include "src/connectivity/wlan/drivers/third_party/nxp/nxpfmac/bus_interface.h"
#include "src/connectivity/wlan/drivers/third_party/nxp/nxpfmac/mlan.h"

namespace wlan::nxpfmac {

class InternalMemAllocator {
  static constexpr uint64_t kMetaDataSignature = 0xDEADBEEFBABEFACE;
  struct HeadMetaData {
    RegionAllocator::Region::UPtr region;
    uint64_t signature;
  };
  struct TailMetaData {
    uint64_t signature;
  };

 public:
  static constexpr size_t kDefaultInternalVmoSize = 512 * 1024;
  ~InternalMemAllocator();
  static zx_status_t Create(BusInterface* bus, size_t vmo_size,
                            std::unique_ptr<InternalMemAllocator>* out_allocator);

  bool GetInternalVmoInfo(uint8_t* buf_ptr, uint32_t* out_vmo_id = nullptr,
                          uint64_t* out_vmo_offset = nullptr);
  void* Alloc(size_t size);
  bool Free(void* mem_ptr);
  void LogStatus();

 private:
  explicit InternalMemAllocator(BusInterface* bus);
  zx::result<uint8_t*> MapInternalVmo(zx_handle_t vmo, uint64_t vmo_size);
  zx_status_t CreateAndPrepareVmo(size_t vmo_size);

  BusInterface* bus_ = nullptr;
  RegionAllocator allocator_;
  zx::vmo vmo_;
  size_t vmo_size_;
  uint8_t* vmo_mapped_addr_ = {};
  uint32_t alloc_alignment_ = 0;
  size_t num_alloc_fails_ = 0;
  size_t num_free_fails_ = 0;
};

}  // namespace wlan::nxpfmac

#endif  // SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_NXP_NXPFMAC_INTERNAL_MEM_ALLOCATOR_H_
