// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_SYSTEM_ULIB_MINI_PROCESS_SUBPROCESS_H_
#define ZIRCON_SYSTEM_ULIB_MINI_PROCESS_SUBPROCESS_H_

#include <zircon/compiler.h>
#include <zircon/syscalls.h>

__BEGIN_CDECLS

// This struct defines the first message that the child process gets.
typedef struct {
  __typeof(zx_handle_close)* handle_close;
  __typeof(zx_object_wait_async)* object_wait_async;
  __typeof(zx_object_wait_one)* object_wait_one;
  __typeof(zx_object_signal)* object_signal;
  __typeof(zx_event_create)* event_create;
  __typeof(zx_profile_create)* profile_create;
  __typeof(zx_channel_create)* channel_create;
  __typeof(zx_channel_read)* channel_read;
  __typeof(zx_channel_write)* channel_write;
  __typeof(zx_process_exit)* process_exit;
  __typeof(zx_object_get_info)* object_get_info;
  __typeof(zx_port_cancel)* port_cancel;
  __typeof(zx_port_create)* port_create;
  __typeof(zx_pager_create)* pager_create;
  __typeof(zx_pager_create_vmo)* pager_create_vmo;
  __typeof(zx_vmo_create_contiguous)* vmo_contiguous_create;
  __typeof(zx_vmo_create_physical)* vmo_physical_create;
  __typeof(zx_vmo_replace_as_executable)* vmo_replace_as_executable;
  __typeof(zx_thread_exit)* thread_exit;
  __typeof(zx_system_get_page_size)* system_get_page_size;
  __typeof(zx_vmo_read)* vmo_read;
  __typeof(zx_vmo_write)* vmo_write;
} minip_ctx_t;

// Subsequent messages and replies are of this format. The |what| parameter is
// transaction friendly so the client can use zx_channel_call().
typedef struct {
  zx_txid_t what;
  zx_status_t status;
  uint64_t data;
} minip_cmd_t;

void minipr_thread_loop(zx_handle_t channel, uintptr_t fnptr);

__END_CDECLS

#endif  // ZIRCON_SYSTEM_ULIB_MINI_PROCESS_SUBPROCESS_H_
