// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <lib/fit/defer.h>
#include <lib/user_copy/user_ptr.h>
#include <platform.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls/iommu.h>
#include <zircon/syscalls/pci.h>
#include <zircon/syscalls/smc.h>
#include <zircon/types.h>

#include <new>

#include <dev/interrupt.h>
#include <dev/iommu.h>
#include <fbl/inline_array.h>
#include <object/bus_transaction_initiator_dispatcher.h>
#include <object/handle.h>
#include <object/interrupt_dispatcher.h>
#include <object/interrupt_event_dispatcher.h>
#include <object/iommu_dispatcher.h>
#include <object/msi_dispatcher.h>
#include <object/msi_interrupt_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/resource.h>
#include <object/vcpu_dispatcher.h>
#include <object/virtual_interrupt_dispatcher.h>
#include <object/vm_object_dispatcher.h>
#include <vm/vm.h>
#include <vm/vm_object_paged.h>
#include <vm/vm_object_physical.h>

#if ARCH_X86
#include <platform/pc/smbios.h>
#endif

#include <lib/syscalls/forward.h>

#include "driver_priv.h"

#define LOCAL_TRACE 0

// zx_status_t zx_vmo_create_contiguous
zx_status_t sys_vmo_create_contiguous(zx_handle_t bti, size_t size, uint32_t alignment_log2,
                                      zx_handle_t* out) {
  LTRACEF("size 0x%zu\n", size);

  if (size == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (alignment_log2 == 0) {
    alignment_log2 = PAGE_SIZE_SHIFT;
  }
  // catch obviously wrong values
  if (alignment_log2 < PAGE_SIZE_SHIFT || alignment_log2 >= (8 * sizeof(uint64_t))) {
    return ZX_ERR_INVALID_ARGS;
  }

  auto up = ProcessDispatcher::GetCurrent();
  zx_status_t status = up->EnforceBasicPolicy(ZX_POL_NEW_VMO);
  if (status != ZX_OK) {
    return status;
  }

  fbl::RefPtr<BusTransactionInitiatorDispatcher> bti_dispatcher;
  status = up->handle_table().GetDispatcherWithRights(*up, bti, ZX_RIGHT_MAP, &bti_dispatcher);
  if (status != ZX_OK) {
    return status;
  }

  auto align_log2_arg = static_cast<uint8_t>(alignment_log2);

  uint64_t vmo_size = 0;
  status = VmObject::RoundSize(size, &vmo_size);
  if (status != ZX_OK) {
    return status;
  }

  // create a vm object
  fbl::RefPtr<VmObjectPaged> vmo;
  status = VmObjectPaged::CreateContiguous(PMM_ALLOC_FLAG_ANY, vmo_size, align_log2_arg, &vmo);
  if (status != ZX_OK) {
    return status;
  }

  // create a Vm Object dispatcher
  KernelHandle<VmObjectDispatcher> kernel_handle;
  zx_rights_t rights;
  status = VmObjectDispatcher::Create(ktl::move(vmo), size,
                                      VmObjectDispatcher::InitialMutability::kMutable,
                                      &kernel_handle, &rights);
  if (status != ZX_OK) {
    return status;
  }

  // create a handle and attach the dispatcher to it
  return up->MakeAndAddHandle(ktl::move(kernel_handle), rights, out);
}

// zx_status_t zx_vmo_create_physical
zx_status_t sys_vmo_create_physical(zx_handle_t hrsrc, zx_paddr_t paddr, size_t size,
                                    zx_handle_t* out) {
  LTRACEF("size 0x%zu\n", size);

  auto up = ProcessDispatcher::GetCurrent();
  zx_status_t status = up->EnforceBasicPolicy(ZX_POL_NEW_VMO);
  if (status != ZX_OK) {
    return status;
  }

  // Memory should be subtracted from the PhysicalAspace allocators, so it's
  // safe to assume that if the caller has access to a resource for this specified
  // region of MMIO space then it is safe to allow the vmo to be created.
  if ((status = validate_resource_mmio(hrsrc, paddr, size)) != ZX_OK) {
    return status;
  }

  status = VmObject::RoundSize(size, &size);
  if (status != ZX_OK) {
    return status;
  }

  // create a vm object
  fbl::RefPtr<VmObjectPhysical> vmo;
  status = VmObjectPhysical::Create(paddr, size, &vmo);
  if (status != ZX_OK) {
    return status;
  }

  // create a Vm Object dispatcher
  KernelHandle<VmObjectDispatcher> kernel_handle;
  zx_rights_t rights;
  status = VmObjectDispatcher::Create(ktl::move(vmo), size,
                                      VmObjectDispatcher::InitialMutability::kMutable,
                                      &kernel_handle, &rights);
  if (status != ZX_OK) {
    return status;
  }

  // create a handle and attach the dispatcher to it
  return up->MakeAndAddHandle(ktl::move(kernel_handle), rights, out);
}

// zx_status_t zx_iommu_create
zx_status_t sys_iommu_create(zx_handle_t resource, uint32_t type, user_in_ptr<const void> desc,
                             size_t desc_size, zx_handle_t* out) {
  zx_status_t status;
  if ((status = validate_resource_kind_base(resource, ZX_RSRC_KIND_SYSTEM,
                                            ZX_RSRC_SYSTEM_IOMMU_BASE)) < 0) {
    return status;
  }

  if (desc_size > ZX_IOMMU_MAX_DESC_LEN) {
    return ZX_ERR_INVALID_ARGS;
  }

  KernelHandle<IommuDispatcher> handle;
  zx_rights_t rights;

  {
    // Copy the descriptor into the kernel and try to create the dispatcher
    // using it.
    fbl::AllocChecker ac;
    ktl::unique_ptr<uint8_t[]> copied_desc(new (&ac) uint8_t[desc_size]);
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
    if ((status = desc.reinterpret<const uint8_t>().copy_array_from_user(copied_desc.get(),
                                                                         desc_size)) != ZX_OK) {
      return status;
    }
    status = IommuDispatcher::Create(type, ktl::unique_ptr<const uint8_t[]>(copied_desc.release()),
                                     desc_size, &handle, &rights);
    if (status != ZX_OK) {
      return status;
    }
  }

  return ProcessDispatcher::GetCurrent()->MakeAndAddHandle(ktl::move(handle), rights, out);
}

#if ARCH_X86
#include <arch/x86/descriptor.h>
#include <arch/x86/ioport.h>

// zx_status_t zx_ioports_request
zx_status_t sys_ioports_request(zx_handle_t hrsrc, uint16_t io_addr, uint32_t len) {
  zx_status_t status;
  if ((status = validate_resource_ioport(hrsrc, io_addr, len)) != ZX_OK) {
    return status;
  }

  LTRACEF("addr 0x%x len 0x%x\n", io_addr, len);

  return IoBitmap::GetCurrent()->SetIoBitmap(io_addr, len, /*enable=*/true);
}

// zx_status_t zx_ioports_release
zx_status_t sys_ioports_release(zx_handle_t hrsrc, uint16_t io_addr, uint32_t len) {
  zx_status_t status;
  if ((status = validate_resource_ioport(hrsrc, io_addr, len)) != ZX_OK) {
    return status;
  }

  LTRACEF("addr 0x%x len 0x%x\n", io_addr, len);

  return IoBitmap::GetCurrent()->SetIoBitmap(io_addr, len, /*enable=*/false);
}

#else
// zx_status_t zx_ioports_request
zx_status_t sys_ioports_request(zx_handle_t hrsrc, uint16_t io_addr, uint32_t len) {
  // doesn't make sense on non-x86
  return ZX_ERR_NOT_SUPPORTED;
}

// zx_status_t zx_ioports_release
zx_status_t sys_ioports_release(zx_handle_t hrsrc, uint16_t io_addr, uint32_t len) {
  return ZX_ERR_NOT_SUPPORTED;
}
#endif

// zx_status_t zx_msi_allocate
zx_status_t sys_msi_allocate(zx_handle_t msi, uint32_t count, zx_handle_t* out) {
  zx_status_t status;
  if ((status = validate_resource_kind_base(msi, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_MSI_BASE)) !=
      ZX_OK) {
    return status;
  }

  fbl::RefPtr<MsiAllocation> alloc;
  if ((status = MsiAllocation::Create(count, &alloc)) != ZX_OK) {
    return status;
  }

  zx_rights_t rights;
  KernelHandle<MsiDispatcher> alloc_handle;
  if ((status = MsiDispatcher::Create(ktl::move(alloc), &alloc_handle, &rights)) != ZX_OK) {
    return status;
  }

  return ProcessDispatcher::GetCurrent()->MakeAndAddHandle(ktl::move(alloc_handle), rights, out);
}

// zx_status_t zx_msi_create
zx_status_t sys_msi_create(zx_handle_t msi_alloc, uint32_t options, uint32_t msi_id,
                           zx_handle_t vmo, size_t vmo_offset, zx_handle_t* out) {
  auto* up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<MsiDispatcher> msi_alloc_disp;

  zx_status_t status;
  if ((status = up->handle_table().GetDispatcher(*up, msi_alloc, &msi_alloc_disp)) != ZX_OK) {
    return status;
  }

  fbl::RefPtr<VmObjectDispatcher> vmo_disp;
  if ((status = up->handle_table().GetDispatcherWithRights(*up, vmo, ZX_RIGHT_MAP, &vmo_disp)) !=
      ZX_OK) {
    return status;
  }

  zx_rights_t rights;
  KernelHandle<InterruptDispatcher> msi_handle;
  if ((status = MsiInterruptDispatcher::Create(
           msi_alloc_disp->msi_allocation(), /* msi_id= */ msi_id, vmo_disp->vmo(),
           /* cap_offset= */ vmo_offset, /* options= */ options, &rights, &msi_handle)) != ZX_OK) {
    return status;
  }
  return up->MakeAndAddHandle(ktl::move(msi_handle), rights, out);
}

// zx_status_t zx_bti_create
zx_status_t sys_bti_create(zx_handle_t iommu, uint32_t options, uint64_t bti_id, zx_handle_t* out) {
  auto up = ProcessDispatcher::GetCurrent();

  if (options != 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  fbl::RefPtr<IommuDispatcher> iommu_dispatcher;
  // TODO(teisenbe): This should probably have a right on it.
  zx_status_t status =
      up->handle_table().GetDispatcherWithRights(*up, iommu, ZX_RIGHT_NONE, &iommu_dispatcher);
  if (status != ZX_OK) {
    return status;
  }

  KernelHandle<BusTransactionInitiatorDispatcher> handle;
  zx_rights_t rights;
  status = BusTransactionInitiatorDispatcher::Create(ktl::move(iommu_dispatcher), bti_id, &handle,
                                                     &rights);
  if (status != ZX_OK) {
    return status;
  }
  return up->MakeAndAddHandle(ktl::move(handle), rights, out);
}

// Helper for optimizing writing many small elements of user ptr array by allowing for a variable
// amount of buffering.
template <typename T, size_t Buf>
class BufferedUserOutPtr {
 public:
  explicit BufferedUserOutPtr(user_out_ptr<T> out_ptr) : out_ptr_(out_ptr) {}
  ~BufferedUserOutPtr() {
    // Ensure Flush was called and everything got written out.
    ASSERT(index_ == 0);
  }
  // Add a single element, either appending to the buffer and/or flushing the buffer if full.
  zx_status_t Write(const T& item) {
    buf_[index_] = item;
    index_++;
    if (index_ == Buf) {
      return Flush();
    }
    return ZX_OK;
  }
  // Flush any remaining buffered items. Must be called prior to destruction.
  zx_status_t Flush() {
    zx_status_t status = out_ptr_.copy_array_to_user(buf_.data(), index_);
    if (status != ZX_OK) {
      return status;
    }
    out_ptr_ = out_ptr_.element_offset(index_);
    index_ = 0;
    return ZX_OK;
  }

 private:
  size_t index_ = 0;
  user_out_ptr<T> out_ptr_;
  ktl::array<T, Buf> buf_;
  // Expectation is this is going to be stack allocated, so ensure it's not too big.
  static_assert(sizeof(buf_) < PAGE_SIZE);
};

// zx_status_t zx_bti_pin
zx_status_t sys_bti_pin(zx_handle_t handle, uint32_t options, zx_handle_t vmo, uint64_t offset,
                        uint64_t size, user_out_ptr<zx_paddr_t> addrs, size_t addrs_count,
                        zx_handle_t* pmt) {
  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<BusTransactionInitiatorDispatcher> bti_dispatcher;
  zx_status_t status =
      up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_MAP, &bti_dispatcher);
  if (status != ZX_OK) {
    return status;
  }

  // Address count is currently limited to the amount of addresses that can fit on 64 pages. This
  // is large enough for all current usage of bti_pin, but protects against the case of an
  // arbitrarily large array being allocated on the heap.
  constexpr size_t kMaxAddrs = (PAGE_SIZE * 64) / sizeof(dev_vaddr_t);
  if (!IS_PAGE_ALIGNED(offset) || !IS_PAGE_ALIGNED(size) || addrs_count > kMaxAddrs) {
    return ZX_ERR_INVALID_ARGS;
  }

  fbl::RefPtr<VmObjectDispatcher> vmo_dispatcher;
  zx_rights_t vmo_rights;
  status = up->handle_table().GetDispatcherAndRights(*up, vmo, &vmo_dispatcher, &vmo_rights);
  if (status != ZX_OK) {
    return status;
  }
  if (!(vmo_rights & ZX_RIGHT_MAP)) {
    return ZX_ERR_ACCESS_DENIED;
  }

  // Convert requested permissions and check against VMO rights
  uint32_t iommu_perms = 0;
  bool compress_results = false;
  bool contiguous = false;
  if (options & ZX_BTI_PERM_READ) {
    if (!(vmo_rights & ZX_RIGHT_READ)) {
      return ZX_ERR_ACCESS_DENIED;
    }
    iommu_perms |= IOMMU_FLAG_PERM_READ;
    options &= ~ZX_BTI_PERM_READ;
  }
  if (options & ZX_BTI_PERM_WRITE) {
    if (!(vmo_rights & ZX_RIGHT_WRITE)) {
      return ZX_ERR_ACCESS_DENIED;
    }
    iommu_perms |= IOMMU_FLAG_PERM_WRITE;
    options &= ~ZX_BTI_PERM_WRITE;
  }
  if (options & ZX_BTI_PERM_EXECUTE) {
    // Note: We check ZX_RIGHT_READ instead of ZX_RIGHT_EXECUTE
    // here because the latter applies to execute permission of
    // the host CPU, whereas ZX_BTI_PERM_EXECUTE applies to
    // transactions initiated by the bus device.
    if (!(vmo_rights & ZX_RIGHT_READ)) {
      return ZX_ERR_ACCESS_DENIED;
    }
    iommu_perms |= IOMMU_FLAG_PERM_EXECUTE;
    options &= ~ZX_BTI_PERM_EXECUTE;
  }
  if (!((options & ZX_BTI_COMPRESS) && (options & ZX_BTI_CONTIGUOUS))) {
    if (options & ZX_BTI_COMPRESS) {
      compress_results = true;
      options &= ~ZX_BTI_COMPRESS;
    }
    if (options & ZX_BTI_CONTIGUOUS && vmo_dispatcher->vmo()->is_contiguous()) {
      contiguous = true;
      options &= ~ZX_BTI_CONTIGUOUS;
    }
  }
  if (options) {
    return ZX_ERR_INVALID_ARGS;
  }

  KernelHandle<PinnedMemoryTokenDispatcher> new_pmt_handle;
  zx_rights_t new_pmt_rights;
  status = bti_dispatcher->Pin(vmo_dispatcher->vmo(), offset, size, iommu_perms, &new_pmt_handle,
                               &new_pmt_rights);
  if (status != ZX_OK) {
    return status;
  }

  // If anything goes wrong from here on out, we _must_ remember to unpin the
  // PMT we are holding.  Failure to do this means that the PMT will hit
  // on-zero-handles while it still has pages pinned and end up in the BTI's
  // quarantine list.  This is definitely not correct as the user never got
  // access to the PMT handle in order to unpin the data.
  //
  // Notice that we're holding a RefPtr to the dispatcher rather than a
  // reference to the |new_pmt_handle|.  Just before we return, |new_pmt_handle|
  // will be moved in order to make a zx_handle_t.  |new_pmt_handle| will
  // not be valid after the move so we keep a RefPtr to the dispatcher instead.
  auto cleanup = fit::defer([disp = new_pmt_handle.dispatcher()]() { disp->Unpin(); });

  static_assert(sizeof(dev_vaddr_t) == sizeof(zx_paddr_t), "mismatched types");
  BufferedUserOutPtr<zx_paddr_t, 32> buffered_addrs(addrs);

  // Define a helper lambda with some state that can fetch potentially large ranges from the PMT,
  // but return them gradually. This just serves as an optimization around repeatedly querying the
  // PMT for a range that it knows is contiguous, but where we need to fill out multiple addresses
  // for the user.
  struct {
    uint64_t offset = 0;
    dev_vaddr_t addr = 0;
    size_t remaining = 0;
  } consume_state;
  auto consume_addr = [&](size_t expected_contig) -> zx::result<dev_vaddr_t> {
    // If the remaining part of the mapping we have cannot satisfy
    if (expected_contig > consume_state.remaining) {
      uint64_t remain = size - consume_state.offset;
      zx_status_t status = new_pmt_handle.dispatcher()->QueryAddress(
          consume_state.offset, remain, &consume_state.addr, &consume_state.remaining);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      if (expected_contig > consume_state.remaining) {
        // This happening suggests an error with contiguity calculations and/or the underlying IOMMU
        // implementation not reporting its contiguity correctly.
        // TODO: consider making this a louder error.
        return zx::error(ZX_ERR_INVALID_ARGS);
      }
    }
    const dev_vaddr_t ret = consume_state.addr;
    consume_state.offset += expected_contig;
    consume_state.addr += expected_contig;
    consume_state.remaining -= expected_contig;
    return zx::ok(ret);
  };

  // Based on the passed in options, determine what size chunks we are going to report to the user,
  // and how many of those there will be.
  size_t target_contig;
  size_t expected_addrs;
  if (compress_results) {
    const size_t min_contig = bti_dispatcher->minimum_contiguity();
    expected_addrs = ROUNDUP(size, min_contig) / min_contig;
    target_contig = min_contig;
  } else if (contiguous) {
    expected_addrs = 1;
    target_contig = size;
  } else {
    expected_addrs = size / PAGE_SIZE;
    target_contig = PAGE_SIZE;
  }
  if (addrs_count != expected_addrs) {
    return ZX_ERR_INVALID_ARGS;
  }
  // Calculate / lookup the addresses.
  for (size_t i = 0; i < addrs_count; i++) {
    const size_t expected_size = ktl::min(size - (target_contig * i), target_contig);
    auto result = consume_addr(expected_size);
    if (result.is_error()) {
      return result.status_value();
    }
    status = buffered_addrs.Write(*result);
    if (status != ZX_OK) {
      return status;
    }
  }

  status = buffered_addrs.Flush();
  if (status != ZX_OK) {
    return status;
  }

  zx_status_t res = up->MakeAndAddHandle(ktl::move(new_pmt_handle), new_pmt_rights, pmt);
  if (res == ZX_OK) {
    cleanup.cancel();
  }

  return res;
}

// zx_status_t zx_bti_release_quarantine
zx_status_t sys_bti_release_quarantine(zx_handle_t handle) {
  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<BusTransactionInitiatorDispatcher> bti_dispatcher;

  zx_status_t status =
      up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_WRITE, &bti_dispatcher);
  if (status != ZX_OK) {
    return status;
  }

  bti_dispatcher->ReleaseQuarantine();
  return ZX_OK;
}

// Having a single-purpose syscall like this is a bit of an anti-pattern in our
// syscall API, but we feel there is benefit in this over trying to extend the
// semantics of handle closing in sys_handle_close and process death.  In
// particular, PMTs are the only objects in the system that track the lifetime
// of something external to the process model (external hardware DMA
// capabilities).
// zx_status_t zx_pmt_unpin
zx_status_t sys_pmt_unpin(zx_handle_t handle) {
  auto up = ProcessDispatcher::GetCurrent();

  HandleOwner handle_owner = up->handle_table().RemoveHandle(*up, handle);
  if (!handle_owner) {
    return ZX_ERR_BAD_HANDLE;
  }

  fbl::RefPtr<Dispatcher> dispatcher = handle_owner->dispatcher();
  auto pmt_dispatcher = DownCastDispatcher<PinnedMemoryTokenDispatcher>(&dispatcher);
  if (!pmt_dispatcher) {
    return ZX_ERR_WRONG_TYPE;
  }

  pmt_dispatcher->Unpin();

  return ZX_OK;
}

// zx_status_t zx_interrupt_create
zx_status_t sys_interrupt_create(zx_handle_t src_obj, uint32_t src_num, uint32_t options,
                                 zx_handle_t* out_handle) {
  LTRACEF("options 0x%x\n", options);

  // resource not required for virtual interrupts
  if (!(options & ZX_INTERRUPT_VIRTUAL)) {
    zx_status_t status;
    if ((status = validate_resource_irq(src_obj, src_num)) != ZX_OK) {
      return status;
    }
  }

  KernelHandle<InterruptDispatcher> handle;
  zx_rights_t rights;
  zx_status_t result;
  if (options & ZX_INTERRUPT_VIRTUAL) {
    result = VirtualInterruptDispatcher::Create(&handle, &rights, options);
  } else {
    result = InterruptEventDispatcher::Create(&handle, &rights, src_num, options);
  }
  if (result != ZX_OK) {
    return result;
  }

  return ProcessDispatcher::GetCurrent()->MakeAndAddHandle(ktl::move(handle), rights, out_handle);
}

// zx_status_t zx_interrupt_bind
zx_status_t sys_interrupt_bind(zx_handle_t handle, zx_handle_t port_handle, uint64_t key,
                               uint32_t options) {
  LTRACEF("handle %x\n", handle);
  if ((options != ZX_INTERRUPT_BIND) && (options != ZX_INTERRUPT_UNBIND)) {
    return ZX_ERR_INVALID_ARGS;
  }

  zx_status_t status;
  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<InterruptDispatcher> interrupt;
  status = up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_READ, &interrupt);
  if (status != ZX_OK) {
    return status;
  }

  fbl::RefPtr<PortDispatcher> port;
  status = up->handle_table().GetDispatcherWithRights(*up, port_handle, ZX_RIGHT_WRITE, &port);
  if (status != ZX_OK) {
    return status;
  }
  if (!port->can_bind_to_interrupt()) {
    return ZX_ERR_WRONG_TYPE;
  }

  if (options == ZX_INTERRUPT_BIND) {
    return interrupt->Bind(ktl::move(port), key);
  } else {
    return interrupt->Unbind(ktl::move(port));
  }
}

// zx_status_t zx_interrupt_ack
zx_status_t sys_interrupt_ack(zx_handle_t inth) {
  LTRACEF("handle %x\n", inth);

  zx_status_t status;
  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<InterruptDispatcher> interrupt;
  status = up->handle_table().GetDispatcherWithRights(*up, inth, ZX_RIGHT_WRITE, &interrupt);
  if (status != ZX_OK) {
    return status;
  }
  return interrupt->Ack();
}

// zx_status_t zx_interrupt_wait
zx_status_t sys_interrupt_wait(zx_handle_t handle, user_out_ptr<zx_time_t> out_timestamp) {
  LTRACEF("handle %x\n", handle);

  zx_status_t status;
  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<InterruptDispatcher> interrupt;
  status = up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_WAIT, &interrupt);
  if (status != ZX_OK) {
    return status;
  }

  zx_time_t timestamp;
  status = interrupt->WaitForInterrupt(&timestamp);
  if (status == ZX_OK && out_timestamp) {
    status = out_timestamp.copy_to_user(timestamp);
  }

  return status;
}

// zx_status_t zx_interrupt_destroy
zx_status_t sys_interrupt_destroy(zx_handle_t handle) {
  LTRACEF("handle %x\n", handle);

  zx_status_t status;
  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<InterruptDispatcher> interrupt;
  status = up->handle_table().GetDispatcher(*up, handle, &interrupt);
  if (status != ZX_OK) {
    return status;
  }

  return interrupt->Destroy();
}

// zx_status_t zx_interrupt_trigger
zx_status_t sys_interrupt_trigger(zx_handle_t handle, uint32_t options, zx_time_t timestamp) {
  LTRACEF("handle %x\n", handle);

  if (options) {
    return ZX_ERR_INVALID_ARGS;
  }

  zx_status_t status;
  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<InterruptDispatcher> interrupt;
  status = up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_SIGNAL, &interrupt);
  if (status != ZX_OK) {
    return status;
  }

  return interrupt->Trigger(timestamp);
}

// zx_status_t zx_smc_call
zx_status_t sys_smc_call(zx_handle_t handle, user_in_ptr<const zx_smc_parameters_t> parameters,
                         user_out_ptr<zx_smc_result_t> out_smc_result) {
  if (!parameters || !out_smc_result) {
    return ZX_ERR_INVALID_ARGS;
  }

  zx_smc_parameters_t params;
  zx_status_t status = parameters.copy_from_user(&params);
  if (status != ZX_OK) {
    return status;
  }

  uint32_t service_call_num = ARM_SMC_GET_SERVICE_CALL_NUM_FROM_FUNC_ID(params.func_id);
  if ((status = validate_resource_smc(handle, service_call_num)) != ZX_OK) {
    return status;
  }

  zx_smc_result_t result;

  status = arch_smc_call(&params, &result);
  if (status != ZX_OK) {
    return status;
  }
  return out_smc_result.copy_to_user(result);
}
