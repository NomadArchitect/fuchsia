// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/asm.h>
#include <arch/arm64/asm.h>
#include <lib/arch/arm64/system-asm.h>

#ifndef __has_feature
#define __has_feature(x) 0
#endif

#if __has_feature(shadow_call_stack)
// void arm64_context_switch(vaddr_t* old_sp, vaddr_t new_sp, vaddr_t new_tpidr,
//                           uintptr_t** old_scsp, uintptr_t* new_scsp);
#else
// void arm64_context_switch(vaddr_t* old_sp, vaddr_t new_sp, vaddr_t new_tpidr);
#endif
FUNCTION(arm64_context_switch)
    /* The exclusive monitor is logically part of a thread's context.  Clear it to ensure the new
       thread does not complete the store half of a load-exclusive/store-exclusive started by the
       old thread.  If the context switch involves an exception return (eret) the monitor will be
       cleared automatically.  However, if the thread has yielded, an eret may not occur and so we
       must clear it here. */
    clrex

    /* save old frame */
    /* This layout should match struct context_switch_frame */
    sub_from_sp (6 * 16)
    str  x19, [sp] // don't save x20, it is the percpu pointer
    stp  x21, x22, [sp, (1 * 16)]
    stp  x23, x24, [sp, (2 * 16)]
    stp  x25, x26, [sp, (3 * 16)]
    stp  x27, x28, [sp, (4 * 16)]
    stp  x29, x30, [sp, (5 * 16)]

    /* save old sp */
    mov  x9, sp
    str  x9, [x0]

    /* load new sp */
    mov  sp, x1

    /* load the new tpidr */
    msr  tpidr_el1, x2

#if __has_feature(shadow_call_stack)
    str shadow_call_sp, [x3]
    mov shadow_call_sp, x4
#endif

    /* restore new frame */
    ldr  x19, [sp] // don't restore x20, it's the percpu pointer
    ldp  x21, x22, [sp, (1 * 16)]
    ldp  x23, x24, [sp, (2 * 16)]
    ldp  x29, x30, [sp, (5 * 16)] // Restore earlier for better scheduling with ret
    ldp  x25, x26, [sp, (3 * 16)]
    ldp  x27, x28, [sp, (4 * 16)]
    add_to_sp (6 * 16)

    ret
    // Prevent speculation through RET
    SPECULATION_POSTFENCE
END_FUNCTION(arm64_context_switch)

// Walk through an array of arm64_sp_info structs (defined in arch.cc), looking for one that
// matches the current cpu's MPIDR. When found, return information from it.
FUNCTION(arm64_get_secondary_sp)
    mrs     x9, mpidr_el1
    movlit  x10, 0xff00ffffff  // Mask for AFFx (cluster) IDs.
    and     x9, x9, x10
    mov     x10, #SMP_MAX_CPUS

    adr_global x11, arm64_secondary_sp_list

.Lsp_loop:
    ldr     x12, [x11, #0] // arm64_sp_info.mpid
    cmp     x12, x9
    beq     .Lsp_found
    add     x11, x11, #40
    subs    x10, x10, #1
    bne     .Lsp_loop
    mov     x0, xzr
    mov     x1, xzr
    mov     x2, xzr
    ret
    SPECULATION_POSTFENCE

.Lsp_found:
    ldr     x0, [x11, #8]  // arm64_sp_info.sp
    ldr     x2, [x11, #16] // arm64_sp_info.shadow_call_sp
    add     x1, x11, #40
    ret
    SPECULATION_POSTFENCE
END_FUNCTION(arm64_get_secondary_sp)
