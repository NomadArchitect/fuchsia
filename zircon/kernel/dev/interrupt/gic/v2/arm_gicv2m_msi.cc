// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2016, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/pow2_range_allocator.h>
#include <pow2.h>
#include <string.h>
#include <sys/types.h>
#include <trace.h>
#include <zircon/types.h>

#include <dev/interrupt/arm_gicv2m.h>
#include <dev/interrupt/arm_gicv2m_msi.h>

#define LOCAL_TRACE 0

namespace {

Pow2RangeAllocator s_32bit_targets;
Pow2RangeAllocator s_64bit_targets;

bool s_msi_initialized = false;

}  // anonymous namespace

zx_status_t arm_gicv2m_msi_init() {
  zx_status_t ret;

  ret = s_32bit_targets.Init(MAX_MSI_IRQS);
  if (ret != ZX_OK) {
    TRACEF("Failed to initialize 32 bit allocation pool!\n");
    return ret;
  }

  ret = s_64bit_targets.Init(MAX_MSI_IRQS);
  if (ret != ZX_OK) {
    TRACEF("Failed to initialize 64 bit allocation pool!\n");
    s_32bit_targets.Free();
    return ret;
  }

  /* TODO(johngro)
   *
   * Right now, the pow2 range allocator will not accept overlapping ranges.
   * It may be possible for fancy GIC implementations to have multiple MSI
   * frames aligned on 4k boundaries (for virtualisation) with either
   * completely or partially overlapping IRQ ranges.  If/when we need to deal
   * with hardware like this, we will need to come back here and make this
   * system more sophisticated.
   */
  arm_gicv2m_frame_info_t info;
  for (uint i = 0; arm_gicv2m_get_frame_info(i, &info) == ZX_OK; ++i) {
    Pow2RangeAllocator* pool =
        (info.doorbell & 0xFFFFFFFF00000000) ? &s_64bit_targets : &s_32bit_targets;

    uint len = info.end_spi_id - info.start_spi_id + 1;
    ret = pool->AddRange(info.start_spi_id, len);
    if (ret != ZX_OK) {
      TRACEF("Failed to add MSI IRQ range [%u, %u] to allocator (ret %d).\n", info.start_spi_id,
             len, ret);
      goto finished;
    }
  }

finished:
  if (ret != ZX_OK) {
    s_32bit_targets.Free();
    s_64bit_targets.Free();
  }

  s_msi_initialized = true;
  return ret;
}

zx_status_t arm_gicv2m_msi_alloc_block(uint requested_irqs, bool can_target_64bit, bool is_msix,
                                       msi_block_t* out_block) {
  if (!out_block)
    return ZX_ERR_INVALID_ARGS;

  if (out_block->allocated)
    return ZX_ERR_BAD_STATE;

  if (!requested_irqs || (requested_irqs > MAX_MSI_IRQS))
    return ZX_ERR_INVALID_ARGS;

  zx_status_t ret = ZX_ERR_INTERNAL;
  bool is_32bit = false;
  uint alloc_size = 1u << log2_uint_ceil(requested_irqs);
  uint alloc_start;

  /* If this MSI request can tolerate a 64 bit target address, start by
   * attempting to allocate from the 64 bit pool */
  if (can_target_64bit)
    ret = s_64bit_targets.AllocateRange(alloc_size, &alloc_start);

  /* No allocation yet?  Fall back on the 32 bit pool */
  if (ret != ZX_OK) {
    ret = s_32bit_targets.AllocateRange(alloc_size, &alloc_start);
    is_32bit = true;
  }

  /* If we have not managed to allocate yet, then we fail */
  if (ret != ZX_OK)
    return ret;

  /* Find the target physical address for this allocation.
   *
   * TODO(johngro) : we could make this O(k) instead of O(n) by associating a
   * context pointer with ranges registered with the pow2 allocator.  Right
   * now, however, N tends to be 1, so it is difficult to be too concerned
   * about this.
   */
  arm_gicv2m_frame_info_t info;
  for (uint i = 0; (ret = arm_gicv2m_get_frame_info(i, &info)) == ZX_OK; ++i) {
    uint alloc_end = alloc_start + alloc_size - 1;

    if (((alloc_start >= info.start_spi_id) && (alloc_start <= info.end_spi_id)) &&
        ((alloc_end >= info.start_spi_id) && (alloc_end <= info.end_spi_id)))
      break;
  }

  /* This should never ever fail */
  DEBUG_ASSERT(ret == ZX_OK);
  if (ret != ZX_OK) {
    Pow2RangeAllocator* pool = is_32bit ? &s_32bit_targets : &s_64bit_targets;
    pool->FreeRange(alloc_start, alloc_size);
    return ret;
  }

  LTRACEF("success: base spi %u size %u\n", alloc_start, alloc_size);

  /* Success!  Fill out the bookkeeping and we are done */
  out_block->is_32bit = is_32bit;
  out_block->base_irq_id = alloc_start;
  out_block->num_irq = alloc_size;
  out_block->tgt_addr = info.doorbell;
  out_block->tgt_data = alloc_start;
  out_block->allocated = true;
  return ZX_OK;
}

bool arm_gicv2m_msi_is_supported() { return s_msi_initialized; }

bool arm_gicv2m_msi_supports_masking() { return s_msi_initialized; }

void arm_gicv2m_msi_free_block(msi_block_t* block) {
  DEBUG_ASSERT(block);
  DEBUG_ASSERT(block->allocated);

  /* We stashed whether or not this came from the 32 bit pool in the platform context pointer */
  Pow2RangeAllocator* pool = block->is_32bit ? &s_32bit_targets : &s_64bit_targets;
  pool->FreeRange(block->base_irq_id, block->num_irq);
  memset(block, 0, sizeof(*block));
}

void arm_gicv2m_msi_register_handler(const msi_block_t* block, uint msi_id, int_handler handler,
                                     void* ctx) {
  DEBUG_ASSERT(block && block->allocated);
  DEBUG_ASSERT(msi_id < block->num_irq);
  zx_status_t status = register_int_handler(block->base_irq_id + msi_id, handler, ctx);
  DEBUG_ASSERT(status == ZX_OK);
}

void arm_gicv2m_msi_mask_unmask(const msi_block_t* block, uint msi_id, bool mask) {
  DEBUG_ASSERT(block && block->allocated);
  DEBUG_ASSERT(msi_id < block->num_irq);
  if (mask) {
    mask_interrupt(block->base_irq_id + msi_id);
  } else {
    unmask_interrupt(block->base_irq_id + msi_id);
  }
}
