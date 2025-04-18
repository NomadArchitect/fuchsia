// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/magma/platform/platform_mmio.h>
#include <lib/magma_service/mock/mock_bus_mapper.h>
#include <lib/magma_service/mock/mock_mmio.h>

#include <gtest/gtest.h>

#include "ppgtt.h"
#include "registers.h"

class TestPerProcessGtt {
 public:
  class MockBusMapping : public magma::PlatformBusMapper::BusMapping {
   public:
    MockBusMapping(uint64_t page_offset, uint64_t page_count)
        : page_offset_(page_offset), phys_addr_(page_count) {}

    uint64_t page_offset() override { return page_offset_; }
    uint64_t page_count() override { return phys_addr_.size(); }
    std::vector<uint64_t>& Get() override { return phys_addr_; }

   private:
    uint64_t page_offset_;
    std::vector<uint64_t> phys_addr_;
  };

  class AddressSpaceOwner : public PerProcessGtt::Owner {
   public:
    virtual ~AddressSpaceOwner() = default;
    magma::PlatformBusMapper* GetBusMapper() override { return &bus_mapper_; }

   private:
    MockBusMapper bus_mapper_;
  };

  static uint32_t cache_bits(CachingType caching_type) {
    switch (caching_type) {
      case CACHING_NONE:
        return (1 << 3) | (1 << 4);  // 3
      case CACHING_WRITE_THROUGH:
        return (1 << 4);  // 3
      case CACHING_LLC:
        return 1 << 7;  // 4
    }
  }

  static void check_pte_entries_clear(PerProcessGtt* ppgtt, uint64_t gpu_addr, uint64_t size) {
    uint64_t page_count = size >> PAGE_SHIFT;

    for (unsigned int i = 0; i < page_count; i++) {
      uint64_t pte = ppgtt->get_pte(gpu_addr + i * PAGE_SIZE);
      EXPECT_EQ(pte & ~(PAGE_SIZE - 1), ppgtt->pml4_table()->scratch_page_bus_addr());
      EXPECT_TRUE(pte & (1 << 0));   // present
      EXPECT_FALSE(pte & (1 << 1));  // not writeable
      EXPECT_EQ(pte & cache_bits(CACHING_NONE), cache_bits(CACHING_NONE));
    }
  }

  static void check_pte_entries(PerProcessGtt* ppgtt,
                                magma::PlatformBusMapper::BusMapping* bus_mapping,
                                uint64_t gpu_addr) {
    auto& bus_addr_array = bus_mapping->Get();

    for (unsigned int i = 0; i < bus_addr_array.size() + PerProcessGtt::kOverfetchPageCount +
                                     PerProcessGtt::kGuardPageCount;
         i++) {
      uint64_t pte = ppgtt->get_pte(gpu_addr + i * PAGE_SIZE);
      if (i < bus_addr_array.size()) {
        EXPECT_EQ(pte & ~(PAGE_SIZE - 1), bus_addr_array[i]);
      } else {
        EXPECT_EQ(pte & ~(PAGE_SIZE - 1), ppgtt->pml4_table()->scratch_page_bus_addr());
      }

      EXPECT_TRUE(pte & (1 << 0));
      EXPECT_EQ(static_cast<bool>(pte & (1 << 1)), i < bus_addr_array.size());  // writeable
      if (i < bus_addr_array.size()) {
        EXPECT_EQ(pte & cache_bits(CACHING_LLC), cache_bits(CACHING_LLC));
      } else {
        EXPECT_EQ(pte & cache_bits(CACHING_NONE), cache_bits(CACHING_NONE));
      }
    }
  }

  static void Init() {
    auto owner = std::make_unique<AddressSpaceOwner>();
    auto ppgtt = PerProcessGtt::Create(owner.get());

    check_pte_entries_clear(ppgtt.get(), (1ull << 48) - PAGE_SIZE, PAGE_SIZE);
    check_pte_entries_clear(ppgtt.get(), (1ull << 47) - PAGE_SIZE, PAGE_SIZE);
    check_pte_entries_clear(ppgtt.get(), (1ull << 40) - PAGE_SIZE, PAGE_SIZE);
    check_pte_entries_clear(ppgtt.get(), (1ull << 33) - PAGE_SIZE, PAGE_SIZE);
    check_pte_entries_clear(ppgtt.get(), (1ull << 32) - PAGE_SIZE, PAGE_SIZE);
    check_pte_entries_clear(ppgtt.get(), (1ull << 31) - PAGE_SIZE, PAGE_SIZE);
    check_pte_entries_clear(ppgtt.get(), 0, 10 * PAGE_SIZE);
  }

  static void Insert(bool with_guard_pages) {
    uint32_t guard_page_count = with_guard_pages ? PerProcessGtt::ExtraPageCount() : 0;

    auto owner = std::make_unique<AddressSpaceOwner>();
    auto ppgtt = PerProcessGtt::Create(owner.get());

    std::vector<uint64_t> addr(2);
    std::vector<std::unique_ptr<magma::PlatformBuffer>> buffer(2);
    std::vector<std::unique_ptr<MockBusMapping>> bus_mapping(2);

    // Placeholder occupies most of the first page directory
    addr[0] = 512 * 511 * PAGE_SIZE + guard_page_count * PAGE_SIZE;
    buffer[0] = magma::PlatformBuffer::Create(513 * PAGE_SIZE, "test");

    addr[1] = addr[0] + buffer[0]->size() + guard_page_count * PAGE_SIZE;
    buffer[1] = magma::PlatformBuffer::Create(10000, "test");

    bus_mapping[0] = std::make_unique<MockBusMapping>(0, buffer[0]->size() / PAGE_SIZE);
    uint64_t phys_addr_base = 0xabcd1000;
    for (auto& phys_addr : bus_mapping[0]->Get()) {
      phys_addr = phys_addr_base += PAGE_SIZE;
    }

    bus_mapping[1] = std::make_unique<MockBusMapping>(0, buffer[1]->size() / PAGE_SIZE);
    for (auto& phys_addr : bus_mapping[1]->Get()) {
      phys_addr = phys_addr_base += PAGE_SIZE;
    }

    EXPECT_TRUE(ppgtt->Insert(addr[0], bus_mapping[0].get()));
    check_pte_entries(ppgtt.get(), bus_mapping[0].get(), addr[0]);

    EXPECT_TRUE(ppgtt->Insert(addr[1], bus_mapping[1].get()));
    check_pte_entries(ppgtt.get(), bus_mapping[1].get(), addr[1]);

    EXPECT_TRUE(ppgtt->Clear(addr[0], bus_mapping[0].get()));
    check_pte_entries_clear(ppgtt.get(), addr[0], buffer[0]->size());
    check_pte_entries(ppgtt.get(), bus_mapping[1].get(), addr[1]);

    EXPECT_TRUE(ppgtt->Clear(addr[1], bus_mapping[1].get()));
    check_pte_entries_clear(ppgtt.get(), addr[1], buffer[1]->size());

    EXPECT_TRUE(ppgtt->Free(addr[0]));
    EXPECT_TRUE(ppgtt->Free(addr[1]));
  }

  static void PrivatePat() {
    auto reg_io = std::make_unique<MsdIntelRegisterIo>(MockMmio::Create(8ULL * 1024 * 1024));

    PerProcessGtt::InitPrivatePat(reg_io.get(), {});

    EXPECT_EQ(0xA0907u, reg_io->Read32(registers::PatIndex::kOffsetLow));
    EXPECT_EQ(0x3B2B1B0Bu, reg_io->Read32(registers::PatIndex::kOffsetHigh));
  }

  static void PrivatePatGen12() {
    auto reg_io = std::make_unique<MsdIntelRegisterIo>(MockMmio::Create(8ULL * 1024 * 1024));

    PerProcessGtt::InitPrivatePatGen12(reg_io.get(), {});

    for (uint32_t i = 0; i < registers::PatIndexGen12::kIndexCount; i++) {
      switch (i) {
        case 1:
          EXPECT_EQ(registers::PatIndexGen12::WRITE_COMBINING,
                    reg_io->Read32(registers::PatIndexGen12::kOffset + i * sizeof(uint32_t)));
          break;
        case 2:
          EXPECT_EQ(registers::PatIndexGen12::WRITE_THROUGH,
                    reg_io->Read32(registers::PatIndexGen12::kOffset + i * sizeof(uint32_t)));
          break;
        case 3:
          EXPECT_EQ(registers::PatIndexGen12::UNCACHEABLE,
                    reg_io->Read32(registers::PatIndexGen12::kOffset + i * sizeof(uint32_t)));
          break;
        default:
          EXPECT_EQ(registers::PatIndexGen12::WRITE_BACK,
                    reg_io->Read32(registers::PatIndexGen12::kOffset + i * sizeof(uint32_t)));
          break;
      }
    }
  }
};

TEST(PerProcessGtt, Init) { TestPerProcessGtt::Init(); }

TEST(PerProcessGtt, InsertWithGuardPages) { TestPerProcessGtt::Insert(true); }

TEST(PerProcessGtt, Insert) { TestPerProcessGtt::Insert(false); }

TEST(PerProcessGtt, PrivatePat) { TestPerProcessGtt::PrivatePat(); }

TEST(PerProcessGtt, PrivatePatGen12) { TestPerProcessGtt::PrivatePatGen12(); }
