// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_FEATURES_H_
#define ZIRCON_FEATURES_H_

// clang-format off

// types of features that can be retrieved via |zx_system_get_features|
#define ZX_FEATURE_KIND_CPU                   ((uint32_t)0)
#define ZX_FEATURE_KIND_HW_BREAKPOINT_COUNT   ((uint32_t)1)
#define ZX_FEATURE_KIND_HW_WATCHPOINT_COUNT   ((uint32_t)2)
#define ZX_FEATURE_KIND_ADDRESS_TAGGING       ((uint32_t)3)
#define ZX_FEATURE_KIND_VM                    ((uint32_t)4)

// arch-independent CPU features
#define ZX_HAS_CPU_FEATURES            ((uint32_t)(1u << 0))

// Virtual memory features.
#define ZX_VM_FEATURE_CAN_MAP_XOM ((uint32_t)(1u << 0))

#if defined(__x86_64__)

// x86-64 CPU features
// None; use cpuid instead

#elif defined(__aarch64__)

// arm64 CPU features
#define ZX_ARM64_FEATURE_ISA_FP        ((uint32_t)(1u << 1))
#define ZX_ARM64_FEATURE_ISA_ASIMD     ((uint32_t)(1u << 2))
#define ZX_ARM64_FEATURE_ISA_AES       ((uint32_t)(1u << 3))
#define ZX_ARM64_FEATURE_ISA_PMULL     ((uint32_t)(1u << 4))
#define ZX_ARM64_FEATURE_ISA_SHA1      ((uint32_t)(1u << 5))
#define ZX_ARM64_FEATURE_ISA_SHA256    ((uint32_t)(1u << 6))
#define ZX_ARM64_FEATURE_ISA_CRC32     ((uint32_t)(1u << 7))
#define ZX_ARM64_FEATURE_ISA_ATOMICS   ((uint32_t)(1u << 8))
#define ZX_ARM64_FEATURE_ISA_RDM       ((uint32_t)(1u << 9))
#define ZX_ARM64_FEATURE_ISA_SHA3      ((uint32_t)(1u << 10))
#define ZX_ARM64_FEATURE_ISA_SM3       ((uint32_t)(1u << 11))
#define ZX_ARM64_FEATURE_ISA_SM4       ((uint32_t)(1u << 12))
#define ZX_ARM64_FEATURE_ISA_DP        ((uint32_t)(1u << 13))
#define ZX_ARM64_FEATURE_ISA_DPB       ((uint32_t)(1u << 14))
#define ZX_ARM64_FEATURE_ISA_FHM       ((uint32_t)(1u << 15))
#define ZX_ARM64_FEATURE_ISA_TS        ((uint32_t)(1u << 16))
#define ZX_ARM64_FEATURE_ISA_RNDR      ((uint32_t)(1u << 17))
#define ZX_ARM64_FEATURE_ISA_SHA512    ((uint32_t)(1u << 18))
#define ZX_ARM64_FEATURE_ISA_I8MM      ((uint32_t)(1u << 19))
#define ZX_ARM64_FEATURE_ISA_SVE       ((uint32_t)(1u << 20))
#define ZX_ARM64_FEATURE_ISA_ARM32     ((uint32_t)(1u << 21)) // Supports ARM32 in EL0.
#define ZX_ARM64_FEATURE_ISA_MOPS      ((uint32_t)(1u << 22))

// This is an obsolete name for the same thing.
#define ZX_ARM64_FEATURE_ISA_SHA2      ZX_ARM64_FEATURE_ISA_SHA256

// arm64 address-tagging features
#define ZX_ARM64_FEATURE_ADDRESS_TAGGING_TBI  ((uint32_t)(1u << 0))

#elif defined(__riscv)

// TODO(mcgrathr): RISC-V CPU features.

#else

#error what architecture?

#endif

#endif // ZIRCON_FEATURES_H_
