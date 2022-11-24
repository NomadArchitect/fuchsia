// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/syscalls/hypervisor.h>

#include <vector>

#include <fuzzer/FuzzedDataProvider.h>

#include "src/virtualization/bin/vmm/arch/x64/decode.h"

namespace {

extern "C" int LLVMFuzzerTestOneInput(const uint8_t* data, size_t size) {
  FuzzedDataProvider provider(data, size);
  std::vector<uint8_t> buffer =
      provider.ConsumeBytes<uint8_t>(provider.ConsumeIntegralInRange<uint32_t>(0, 32));
  InstructionSpan span(buffer.data(), buffer.size());
  uint8_t default_operand_size = provider.ConsumeBool() ? 2 : 4;
  zx_vcpu_state_t vcpu_state = {
      provider.ConsumeIntegral<uint64_t>(), provider.ConsumeIntegral<uint64_t>(),
      provider.ConsumeIntegral<uint64_t>(), provider.ConsumeIntegral<uint64_t>(),
      provider.ConsumeIntegral<uint64_t>(), provider.ConsumeIntegral<uint64_t>(),
      provider.ConsumeIntegral<uint64_t>(), provider.ConsumeIntegral<uint64_t>(),
      provider.ConsumeIntegral<uint64_t>(), provider.ConsumeIntegral<uint64_t>(),
      provider.ConsumeIntegral<uint64_t>(), provider.ConsumeIntegral<uint64_t>(),
      provider.ConsumeIntegral<uint64_t>(), provider.ConsumeIntegral<uint64_t>(),
      provider.ConsumeIntegral<uint64_t>(), provider.ConsumeIntegral<uint64_t>(),
      provider.ConsumeIntegral<uint64_t>(),
  };
  [[maybe_unused]] auto inst = DecodeInstruction(span, default_operand_size, vcpu_state);
  return 0;
}

}  // namespace
