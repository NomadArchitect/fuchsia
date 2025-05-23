#include <assert.h>
#include <threads.h>

#include "futex_impl.h"

enum {
  STATE_INIT = 0,  // we're the first; run init
  STATE_WAIT = 1,  // another thread is running init; wait
  STATE_DONE = 2,  // another thread finished running init; just return
  STATE_WAKE = 3,  // another thread is running init, waiters present; wait
};

static_assert(STATE_INIT == ONCE_FLAG_INIT, "");

// This implementation uses memory_order_seq_cst for all access
// to |control|. This is stronger than this use case requires -
// if we update the code to use C's atomic library directly instead
// of the compatibility wrappers like a_cas_shim we could relax this
// to use memory_order_acq_rel.

static void once_full(once_flag* control, void (*init)(void)) {
  for (;;)
    switch (a_cas_shim(control, STATE_INIT, STATE_WAIT)) {
      case STATE_INIT:
        init();

        if (atomic_exchange(control, STATE_DONE) == STATE_WAKE)
          _zx_futex_wake(control, UINT32_MAX);
        return;
      case STATE_WAIT:
        /* If this fails, so will __wait. */
        a_cas_shim(control, STATE_WAIT, STATE_WAKE);
      case STATE_WAKE:
        __wait(control, NULL, STATE_WAKE);
        continue;
      case STATE_DONE:
        return;
    }
}

void call_once(once_flag* control, void (*init)(void)) {
  if (atomic_load(control) == STATE_DONE) {
    return;
  }
  once_full(control, init);
}
