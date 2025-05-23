// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_H_

#include <lib/fit/result.h>
#include <sys/types.h>
#include <zircon/listnode.h>

#include <fbl/algorithm.h>
#include <fbl/macros.h>
#include <kernel/event.h>
#include <kernel/lockdep.h>
#include <kernel/mutex.h>
#include <kernel/semaphore.h>
#include <ktl/array.h>
#include <ktl/optional.h>
#include <ktl/variant.h>
#include <vm/debug_compressor.h>
#include <vm/page.h>

class VmCowPages;

// Allocated pages that are part of the cow pages in a VmObjectPaged can be placed in a page queue.
// The page queues provide a way to
//  * Classify and group pages across VMO boundaries
//  * Retrieve the VMO that a page is contained in (via a back reference stored in the vm_page_t)
// Once a page has been placed in a page queue its queue_node becomes owned by the page queue and
// must not be used until the page has been Remove'd. It is not sufficient to call list_delete on
// the queue_node yourself as this operation is not atomic and needs to be performed whilst holding
// the PageQueues::lock_.
class PageQueues {
 public:
  // The number of reclamation queues is slightly arbitrary, but to be useful you want at least 3
  // representing
  //  * Very new pages that you probably don't want to evict as doing so probably implies you are in
  //    swap death
  //  * Slightly old pages that could be evicted if needed
  //  * Very old pages that you'd be happy to evict
  // With two active queues 8 page queues are used so that there is some fidelity of information in
  // the inactive queues. Additional queues have reduced value as sufficiently old pages quickly
  // become equivalently unlikely to be used in the future.
  static constexpr size_t kNumReclaim = 8;

  // Two active queues are used to allow for better fidelity of active information. This prevents
  // a race between aging once and needing to collect/harvest age information.
  static constexpr size_t kNumActiveQueues = 2;

  static_assert(kNumReclaim > kNumActiveQueues, "Needs to be at least one non-active queue");

  // In addition to active and inactive, we want to consider some of the queues as 'oldest' to
  // provide an additional way to limit eviction. Presently the processing of the LRU queue to make
  // room for aging is not integrated with the Evictor, and so will not trigger eviction, therefore
  // to have a non-zero number of pages ever appear in an oldest queue for eviction the last two
  // queues are considered the oldest.
  static constexpr size_t kNumOldestQueues = 2;
  static_assert(kNumOldestQueues + kNumActiveQueues <= kNumReclaim);

  static constexpr zx_duration_mono_t kDefaultMinMruRotateTime = ZX_SEC(5);
  static constexpr zx_duration_mono_t kDefaultMaxMruRotateTime = ZX_SEC(5);

  // This is presently an arbitrary constant, since the min and max mru rotate time are currently
  // fixed at the same value, meaning that the active ratio can not presently trigger, or prevent,
  // aging.
  static constexpr uint64_t kDefaultActiveRatioMultiplier = 0;

  // When holding the PageQueue lock, and performing an operation on an
  // arbitrary number of pages, the "max batch size" controls the maximum number
  // of number of pages for which the lock will be held before letting it go and
  // allowing other operations to proceed.
  //
  // For example, if someone is calling RemoveArrayIntoList, with a list of 200
  // pages to release, the lock will be obtained and dropped a total of 4 times,
  // removing batches of 64 pages for the first 3 iterations, and finally a
  // batch of 8 pages for the final iteration.
  static inline constexpr size_t kMaxBatchSize = 64;

  PageQueues();
  ~PageQueues();

  DISALLOW_COPY_ASSIGN_AND_MOVE(PageQueues);

  // This is a specialized version of MarkAccessed designed to be called during accessed harvesting.
  // It does not update active/inactive counts, and this needs to be done separately once harvesting
  // is complete. It is only permitted to call this in between BeginAccessScan and EndAccessScan
  // calls.
  void MarkAccessedDeferredCount(vm_page_t* page);

  // All Set operations places a page, which must not currently be in a page queue, into the
  // specified queue. The backlink information of |object| and |page_offset| must be specified and
  // valid. If the page is either removed from the referenced object, or moved to a different
  // offset, the backlink information must be updated either by calling ChangeObjectOffsetLocked, or
  // removing the page completely from the queues.

  void SetWired(vm_page_t* page, VmCowPages* object, uint64_t page_offset);
  void SetAnonymous(vm_page_t* page, VmCowPages* object, uint64_t page_offset);
  void SetReclaim(vm_page_t* page, VmCowPages* object, uint64_t page_offset);
  void SetPagerBackedDirty(vm_page_t* page, VmCowPages* object, uint64_t page_offset);
  void SetAnonymousZeroFork(vm_page_t* page, VmCowPages* object, uint64_t page_offset);
  void SetHighPriority(vm_page_t* page, VmCowPages* object, uint64_t page_offset);

  // All Move operations change the queue that a page is considered to be in, but do not change the
  // object or offset backlink information. The page must currently be in a valid page queue.

  void MoveToWired(vm_page_t* page);
  void MoveToAnonymous(vm_page_t* page);
  void MoveToReclaim(vm_page_t* page);
  void MoveToReclaimDontNeed(vm_page_t* page);
  void MoveToPagerBackedDirty(vm_page_t* page);
  void MoveToAnonymousZeroFork(vm_page_t* page);
  void MoveToHighPriority(vm_page_t* page);

  // Indicates that page has failed a compression attempted, and moves it to a separate queue to
  // prevent it from being considered part of the reclaim set, which makes it neither active nor
  // inactive. The specified page must be in the page queues, but if not presently in a reclaim
  // queue this method will do nothing.
  // TODO(https://fxbug.dev/42138396): Determine whether/how pages are moved back into the reclaim
  // pool and either further generalize this to support pager backed, or specialize FailedReclaim to
  // be explicitly only anonymous.
  void CompressFailed(vm_page_t* page);

  // Changes the backlink information for a page and should only be called by the page owner under
  // its lock (that is the VMO lock). The page must currently be in a valid page queue.
  void ChangeObjectOffset(vm_page_t* page, VmCowPages* object, uint64_t page_offset);
  void ChangeObjectOffsetArray(vm_page_t** pages, VmCowPages* object, uint64_t* offsets,
                               size_t count);

  // Externally locked variant of CHangeObjectOffset that can be used for more efficient batch
  // operations. In addition to the annotated lock_, the VMO lock of the owner is also required to
  // be held.
  void ChangeObjectOffsetLocked(vm_page_t* page, VmCowPages* object, uint64_t page_offset)
      TA_REQ(lock_);

  // Removes the page from any page list and returns ownership of the queue_node.
  void Remove(vm_page_t* page);
  // Batched version of Remove that also places all the pages in the specified list
  void RemoveArrayIntoList(vm_page_t** page, size_t count, list_node_t* out_list);

  // Tells the page queue this page has been accessed, and it should have its position in the queues
  // updated. This method will take the internal page queues lock and should not be used for
  // accessed harvesting, where MarkAccessedDeferredCount should be used instead.
  void MarkAccessed(vm_page_t* page) {
    // Can retrieve the queue ref and do a short circuit check for whether mark accessed is
    // necessary. Although this check is racy, since we do not yet hold the lock, we will either:
    //  * Race and fail to mark accessed - this is fine since racing implies a newly added page,
    //    which would be added to the mru queue anyway.
    //  * Race and attempt to spuriously mark accessed - this is fine as we will check again with
    //    the lock held.
    auto queue_ref = page->object.get_page_queue_ref();
    const uint8_t queue = queue_ref.load(ktl::memory_order_relaxed);
    if (queue < PageQueueReclaimDontNeed) {
      return;
    }
    // With the early check complete, continue with the non-inlined longer body.
    MarkAccessedContinued(page);
  }

  // Provides access to the underlying lock, allowing _Locked variants to be called. Use of this is
  // highly discouraged as the underlying lock is a CriticalMutex which disables preemption.
  // Preferably *Array variations should be used, but this provides a higher performance mechanism
  // when needed.
  Lock<SpinLock>* get_lock() TA_RET_CAP(lock_) { return &lock_; }

  // Used to identify the reason that aging is triggered, mostly for debugging and informational
  // purposes.
  enum class AgeReason {
    // Aging occurred due to the maximum timeout being reached before any other reason could trigger
    Timeout,
    // The allowable ratio of active versus inactive pages was exceeded.
    ActiveRatio,
    // An explicit call to RotatePagerBackedQueues caused aging. This would typically occur due to
    // test code or via the kernel debug console.
    Manual,
  };
  static const char* string_from_age_reason(PageQueues::AgeReason reason);

  // Rotates the reclamation queues to perform aging. Every existing queue is now considered to be
  // one epoch older. To achieve these two things are done:
  //   1. A new queue, representing the current epoch, needs to be allocated to put pages that get
  //      accessed from here into. This just involves incrementing the MRU generation.
  //   2. As there is a limited number of page queues 'allocating' one might involve cleaning up an
  //      old queue. See the description of ProcessDontNeedAndLruQueues for how this process works.
  void RotateReclaimQueues(AgeReason reason = AgeReason::Manual);

  // Used to represent and return page backlink information acquired whilst holding the page queue
  // lock. As a VMO may not destruct while it has pages in it, the cow RefPtr will always be valid,
  // although the page and offset contained here are not synchronized and must be separately
  // validated before use. This can be done by acquiring the returned vmo's lock and then validating
  // that the page is still contained at the offset.
  struct VmoBacklink {
    fbl::RefPtr<VmCowPages> cow;
    vm_page_t* page = nullptr;
    uint64_t offset = 0;
  };

  // Moves a page from from the anonymous zero fork queue into the anonymous queue and returns
  // the backlink information. If the zero fork queue is empty then a nullopt is returned, otherwise
  // if it has_value the vmo field may be null to indicate that the vmo is running its destructor
  // (see VmoBacklink for more details).
  ktl::optional<VmoBacklink> PopAnonymousZeroFork();

  // Looks at the reclaimable queues and returns backlink information of the first page found. The
  // queues themselves are walked from the current LRU queue up to the queue that is at most
  // |lowest_queue| epochs from the most recent. |lowest_queue| therefore represents the youngest
  // age that would be accepted. If no page was found a nullopt is returned, otherwise if
  // it has_value the vmo field may be null to indicate that the vmo is running its destructor (see
  // VmoBacklink for more details). If a page is returned its location in the reclaim queue is
  // not modified.
  ktl::optional<VmoBacklink> PeekReclaim(size_t lowest_queue);

  // Can be called while the |page| is known to be in the loaned state. This method checks if it is
  // in the page queues, and if so returns a reference to the cow pages that owns it.
  // The page must be 'owned' by the caller, in so far as the page->state() is guaranteed to not be
  // changing.
  ktl::optional<VmoBacklink> GetCowForLoanedPage(vm_page_t* page);

  // Helper struct to group reclaimable queue length counts returned by GetReclaimCounts.
  struct ReclaimCounts {
    size_t total = 0;
    size_t newest = 0;
    size_t oldest = 0;
  };

  // Returns just the reclaim queue counts. Called from the zx_object_get_info() syscall.
  ReclaimCounts GetReclaimQueueCounts() const;

  // Helper struct to group queue length counts returned by QueueCounts.
  struct Counts {
    ktl::array<size_t, kNumReclaim> reclaim = {0};
    size_t reclaim_dont_need = 0;
    size_t pager_backed_dirty = 0;
    size_t anonymous = 0;
    size_t wired = 0;
    size_t anonymous_zero_fork = 0;
    size_t failed_reclaim = 0;
    size_t high_priority = 0;

    bool operator==(const Counts& other) const {
      return reclaim == other.reclaim && reclaim_dont_need == other.reclaim_dont_need &&
             pager_backed_dirty == other.pager_backed_dirty && anonymous == other.anonymous &&
             wired == other.wired && anonymous_zero_fork == other.anonymous_zero_fork &&
             failed_reclaim == other.failed_reclaim && high_priority == other.high_priority;
    }
    bool operator!=(const Counts& other) const { return !(*this == other); }
  };

  Counts QueueCounts() const;

  struct ActiveInactiveCounts {
    // Whether the returned counts were cached values, or the current 'true' values. Cached values
    // are returned if an accessed scan is ongoing, as the true values cannot be determined in a
    // race free way.
    bool cached = false;
    // Pages that would normally be available for eviction, but are presently considered active and
    // so will not be evicted.
    size_t active = 0;
    // Pages that are available for eviction due to not presently being considered active.
    size_t inactive = 0;

    bool operator==(const ActiveInactiveCounts& other) const {
      return cached == other.cached && active == other.active && inactive == other.inactive;
    }
    bool operator!=(const ActiveInactiveCounts& other) const { return !(*this == other); }
  };

  // Retrieves the current active/inactive counts, or a cache of the last known good ones if
  // accessed harvesting is happening. This method is guaranteed to return in a small window of time
  // due to only needing to acquire a single lock that has very short critical sections. However,
  // this means it may have to return old values if accessed scanning is happening. If blocking and
  // waiting is acceptable then |scanner_synchronized_active_inactive_counts| should be used, which
  // calls this when it knows accessed scanning is not happening, guaranteeing a live value.
  ActiveInactiveCounts GetActiveInactiveCounts() const TA_EXCL(lock_);

  void Dump() TA_EXCL(lock_);

  // Returns a global count of all pages compressed at the point of LRU change. This is a global
  // method and will include stats from every PageQueues that has been instantiated.
  static uint64_t GetLruPagesCompressed();

  // Enables reclamation of anonymous pages by causing them to be placed into the reclaimable queue
  // instead of the dedicated anonymous queue. The |zero_forks| parameter controls whether the
  // anonymous zero forks should also go into the general reclaimable queue or not.
  // Any pages already placed into the anonymous queues will be moved over, and there is no way to
  // disable this once enabled.
  void EnableAnonymousReclaim(bool zero_forks);

  // Returns whether or not the reclaim queues only include pager backed pages or not.
  bool ReclaimIsOnlyPagerBacked() const { return !anonymous_is_reclaimable_; }

  // These query functions are marked Debug as it is generally a racy way to determine a pages state
  // and these are exposed for the purpose of writing tests or asserts against the pagequeue.

  // This takes an optional output parameter that, if the function returns true, will contain the
  // index of the queue that the page was in.
  bool DebugPageIsReclaim(const vm_page_t* page, size_t* queue = nullptr) const;
  bool DebugPageIsReclaimDontNeed(const vm_page_t* page) const;
  bool DebugPageIsPagerBackedDirty(const vm_page_t* page) const;
  bool DebugPageIsAnonymous(const vm_page_t* page) const;
  bool DebugPageIsAnonymousZeroFork(const vm_page_t* page) const;
  bool DebugPageIsAnyAnonymous(const vm_page_t* page) const;
  bool DebugPageIsWired(const vm_page_t* page) const;
  bool DebugPageIsHighPriority(const vm_page_t* page) const;

  // These methods are public so that the scanner can call. Once the scanner is an object that can
  // be friended, and not a collection of anonymous functions, these can be made private.

  // Creates any threads for queue management. This needs to be done separately to construction as
  // there is a recursive dependency where creating threads will need to manipulate pages, which
  // will call back into the page queues.
  // Delaying thread creation is fine as these threads are purely for aging and eviction management,
  // which is not needed during early kernel boot.
  // Failure to start the threads may cause operations such as RotatePagerBackedQueues to block
  // indefinitely as they might attempt to offload work to a nonexistent thread. This issue is only
  // relevant for unittests that may wish to avoid starting the threads for some tests.
  // It is the responsibility of the caller to only call this once, otherwise it will panic.
  void StartThreads(zx_duration_mono_t min_mru_rotate_time, zx_duration_mono_t max_mru_rotate_time);

  // Initializes and starts the debug compression, which attempts to immediately compress a random
  // subset of pages added to the page queues. It is an error to call this if there is no compressor
  // or if not running in debug mode.
  void StartDebugCompressor();

  // Sets the active ratio multiplier.
  void SetActiveRatioMultiplier(uint32_t multiplier);

  // Describes any action to take when processing the LRU queue. This is applied to pages that would
  // otherwise have to be moved from the old LRU queue into the next queue when making space.
  enum class LruAction {
    None,
    EvictOnly,
    CompressOnly,
    EvictAndCompress,
  };
  void SetLruAction(LruAction action);

  // Controls to enable and disable the active aging system. These must be called alternately and
  // not in parallel. That is, it is an error to call DisableAging twice without calling EnableAging
  // in between. Similar for EnableAging.
  void DisableAging() TA_EXCL(lock_);
  void EnableAging() TA_EXCL(lock_);

  // Register an Event that will be signalled every time aging occurs. This can be used to know if
  // if PeekReclaim might now return items (due to aging having occurred) where it had previously
  // ceased.
  // Only a single Event may be registered at a time and the Event is assumed to live as long as the
  // PageQueues object. A nullptr can be passed in to unregister an Event, otherwise it is an error
  // to attempt to register over the top of an existing event.
  void SetAgingEvent(Event* event);

  // Called by the scanner to indicate the beginning of an accessed scan. This allows
  // MarkAccessedDeferredCount, and will cause the active/inactive counts returned by
  // GetActiveInactiveCounts to remain unchanged until the accessed scan is complete.
  void BeginAccessScan();
  void EndAccessScan();

 private:
  // An enumeration of the various types of signals which could become
  // pending-dispatch while we are holding the main PageQueues lock.
  enum class PendingSignal : uint32_t {
    None = 0x0,
    AgingToken = 0x1,
    AgingActiveRatioEvent = 0x2,
    LruEvent = 0x4,
  };

  // Specifies the indices for both the page_queues_ and the page_queue_counts_
  enum PageQueue : uint8_t {
    PageQueueNone = 0,
    PageQueueAnonymous,
    PageQueueWired,
    PageQueueHighPriority,
    PageQueueAnonymousZeroFork,
    PageQueuePagerBackedDirty,
    PageQueueFailedReclaim,
    PageQueueReclaimDontNeed,
    PageQueueReclaimBase,
    PageQueueReclaimLast = PageQueueReclaimBase + kNumReclaim - 1,
    PageQueueNumQueues,
  };

  // A "DeferPendingSignals" is a small RAII-style helper which assists with
  // following the rules for properly handling the Events in `PageQueues`
  //
  // As pages are added to, removed from, and moved between, various page
  // queues, it may become necessary to signal and wake up one of the background
  // threads responsible for maintaining the queues.  Most of the time, however,
  // this happens while we are holding the PagesQueues spinlock.  Unfortunately,
  // it is illegal to signal an Event while holding a spinlock, so the
  // signalling operation must be deferred until just after the lock has been
  // dropped..
  //
  // A DeferPendingSignals is used to accumulate the set of Events which
  // need to be signalled when we (eventually) drop the lock.  Methods which
  // might need to have a signal asserted demand that callers pass a reference
  // to a DeferPendingSignals in order to accumulate any pending signals, if
  // needed.
  //
  // PageQueues Events have been wrapped in a thin wrapper class which allows
  // them to be waited on, but not signalled by anything but a
  // DeferPendingSignals.  The PSDs themselves cannot be constructed while
  // the main `lock_` is being held, nor can they be copied, moved, or heap
  // allocated.  Provided that scope based guards are being used to manage the
  // `lock_`, this makes it very difficult for a PSD instance to destruct and
  // flush pending signals while `lock_` is being held.
  //
  // Any time that a user wants to call a locked method which might need a
  // signal to fire, all they need to do is to put a PSD instance on the stack
  // outside of the scope of the lock guard, and pass it to the method they want
  // to call.  For example:
  //
  // ```
  // {
  //   DeferPendingSignals dps{*this};
  //   Guard<SpinLock, IrqSave> guard{&lock_};
  //   RecalculateActiveInactiveLocked(dps);
  // }  // pending signals auto-flushed here, after dropping the lock
  //
  // ```
  //
  class DeferPendingSignals {
   public:
    // We cannot create a PSD while holding its PageQueues' lock instance.
    [[nodiscard]] explicit DeferPendingSignals(PageQueues& page_queues) TA_EXCL(page_queues.lock_)
        : page_queues_(page_queues) {}

    // No move, no copy.
    DeferPendingSignals(const DeferPendingSignals&) = delete;
    DeferPendingSignals(DeferPendingSignals&&) = delete;
    DeferPendingSignals& operator=(const DeferPendingSignals&) = delete;
    DeferPendingSignals& operator=(DeferPendingSignals&&) = delete;

    // When the PSD destructs, fire any pending signals.
    ~DeferPendingSignals() {
      using T = ktl::underlying_type_t<PendingSignal>;
      T pending = static_cast<T>(pending_signals_);

      if (pending) {
        if (pending & static_cast<T>(PendingSignal::AgingToken)) {
          page_queues_.aging_token_.Signal();
        }

        if (pending & static_cast<T>(PendingSignal::AgingActiveRatioEvent)) {
          page_queues_.aging_active_ratio_event_.Signal();
        }

        if (pending & static_cast<T>(PendingSignal::LruEvent)) {
          page_queues_.lru_event_.Signal();
        }
      }
    }

    // Accumulate `signal` into the set of currently pending signals for this
    // dispatcher instance.
    void Pend(PendingSignal signal) {
      using T = ktl::underlying_type_t<PendingSignal>;
      pending_signals_ =
          static_cast<PendingSignal>(static_cast<T>(pending_signals_) | static_cast<T>(signal));
    }

   private:
    // PSDs may not be heap allocated.  We want their scopes to always be
    // defined to a function/stack scope, and never extend outside of that.
    static void* operator new(size_t) = delete;
    static void* operator new[](size_t) = delete;
    static void operator delete(void*) = delete;
    static void operator delete[](void*) = delete;

    PageQueues& page_queues_;
    PendingSignal pending_signals_{PendingSignal::None};
  };

  // a PendingSignalEvent is a thin wrapper around an AutounsignalEvent.  It
  // restricts the API of the AutounsignalEvent just a bit, allowing
  // DeferPendingSignalss to signal events (outside of the `lock_`, as the
  // PSD goes out of scope), but not other code.  This should make it more
  // difficult for someone to accidentally signal one of the events while
  // holding the PageQueues' spinlock.
  class PendingSignalEvent : protected AutounsignalEvent {
   public:
    PendingSignalEvent() = default;
    explicit PendingSignalEvent(bool initial_state) : AutounsignalEvent(initial_state) {}

    using AutounsignalEvent::Unsignal;
    using AutounsignalEvent::Wait;
    using AutounsignalEvent::WaitDeadline;

   private:
    friend class DeferPendingSignals;
  };

  // A DeferPendingSignals is allowed to access the various Event signals.
  friend class DeferPendingSignals;

  // Ensure that the reclaim queue counts are always at the end.
  static_assert(PageQueueReclaimLast + 1 == PageQueueNumQueues);

  // The page queue index, unlike the full generation count, needs to be able to fit inside a
  // uint8_t in the vm_page_t.
  static_assert(PageQueueNumQueues < 256);

  // Converts free running generation to reclaim queue.
  static constexpr PageQueue gen_to_queue(uint64_t gen) {
    return static_cast<PageQueue>((gen % kNumReclaim) + PageQueueReclaimBase);
  }

  // Checks if a candidate reclaim page queue would be valid given a specific lru and mru
  // queue.
  static constexpr bool queue_is_valid(PageQueue page_queue, PageQueue lru, PageQueue mru) {
    DEBUG_ASSERT(page_queue >= PageQueueReclaimBase);
    if (lru <= mru) {
      return page_queue >= lru && page_queue <= mru;
    } else {
      return page_queue <= mru || page_queue >= lru;
    }
  }

  // Returns whether this queue is reclaimable, and hence can be active or inactive. If this
  // returns false then it is guaranteed that both |queue_is_active| and |queue_is_inactive| would
  // return false.
  static constexpr bool queue_is_reclaim(PageQueue page_queue) {
    // We check against the the DontNeed queue and not the base queue so that accessing a page can
    // move it from the DontNeed list into the LRU queues. To keep this case efficient we require
    // that the DontNeed queue be directly before the LRU queues.
    static_assert(PageQueueReclaimDontNeed + 1 == PageQueueReclaimBase);

    // Ensure that the Dirty queue comes before the smallest queue that would return true for this
    // function. This function is used for computing active/inactive sets for the purpose of
    // eviction, and dirty pages cannot be evicted. The Dirty queue also needs to come before the
    // DontNeed queue so that MarkAccessed does not try to move the page to the MRU queue on
    // access.
    static_assert(PageQueuePagerBackedDirty < PageQueueReclaimDontNeed);

    return page_queue >= PageQueueReclaimDontNeed;
  }

  // Calculates the age of a queue against a given mru, with 0 meaning page_queue==mru
  // This is only meaningful to call on reclaimable queues.
  static constexpr uint queue_age(PageQueue page_queue, PageQueue mru) {
    DEBUG_ASSERT(page_queue >= PageQueueReclaimBase);
    if (page_queue <= mru) {
      return mru - page_queue;
    } else {
      return (static_cast<uint>(kNumReclaim) - page_queue) + mru;
    }
  }

  // Returns whether the given page queue would be considered active against a given mru.
  // This is valid to call on any page queue, not just reclaimable ones, and as such this returning
  // false does not imply the queue is inactive.
  static constexpr bool queue_is_active(PageQueue page_queue, PageQueue mru) {
    if (page_queue < PageQueueReclaimBase) {
      return false;
    }
    return queue_age(page_queue, mru) < kNumActiveQueues;
  }

  // Returns whether the given page queue would be considered inactive against a given mru.
  // This is valid to call on any page queue, not just reclaimable ones, and as such this returning
  // false does not imply the queue is active.
  static constexpr bool queue_is_inactive(PageQueue page_queue, PageQueue mru) {
    // The DontNeed queue does not have an age, and so we cannot call queue_age on it, but it should
    // definitely be considered part of the inactive set.
    if (page_queue == PageQueueReclaimDontNeed) {
      return true;
    }
    if (page_queue < PageQueueReclaimBase) {
      return false;
    }
    return queue_age(page_queue, mru) >= kNumActiveQueues;
  }

  PageQueue mru_gen_to_queue() const {
    return gen_to_queue(mru_gen_.load(ktl::memory_order_relaxed));
  }

  PageQueue lru_gen_to_queue() const {
    return gen_to_queue(lru_gen_.load(ktl::memory_order_relaxed));
  }

  // This processes the DontNeed queue and the LRU queue.
  // For the DontNeed queue the pages are either process to termination (if peek is false), or if
  // peek is true we will either return a page from it or make it empty. For the LRU queue, the aim
  // is to make the lru_gen_ be the passed in target_gen. It achieves this by walking all the pages
  // in the queue and either
  //   1. For pages that have a newest accessed time and are in the wrong queue, are moved into the
  //      correct queue.
  //   2. For pages that are in the correct queue, they are either returned (if |peek| is true), or
  //      moved to the next queue, which causes their age to be effectively decreased.
  // In the second case for LRU, pages get moved into the next queue so that the LRU queue can
  // become empty, allowing the gen to be incremented to eventually reach the |target_gen|. The
  // mechanism of freeing up the LRU queue is necessary to make room for new MRU queues. When |peek|
  // is false, this always returns a nullopt and guarantees that it moved lru_gen_ to at least
  // target_gen. If |peek| is true, then the first time it hits a page in case (2), it returns it
  // instead of decreasing its age.
  ktl::optional<PageQueues::VmoBacklink> ProcessDontNeedAndLruQueues(uint64_t target_gen,
                                                                     bool peek);

  // Helper used by ProcessDontNeedAndLruQueues. |target_gen| is the minimum value lru_gen_ should
  // advance to. If |peek| is true, the first page that  encountered in the respective queue, whose
  // age does not require to be fixed up, is returned.
  // The passed in LruIsolate object is used to process reclamation work outside of the lock and can
  // be reused across multiple calls. The |Items| parameter therefore controls how much work will be
  // done within the lock acquisition before returning.
  template <size_t Items>
  class LruIsolate;
  template <size_t Items>
  ktl::optional<PageQueues::VmoBacklink> ProcessLruQueueHelper(LruIsolate<Items>& deferred_list,
                                                               uint64_t target_gen, bool peek)
      TA_EXCL(lock_);

  // Helper used by ProcessDontNeedAndLruQueues. Processes the DontNeed list and processes items
  // into their correct list, and either processes all elements (if peek is false), or returns the
  // first DontNeed item if peek is true.
  ktl::optional<VmoBacklink> ProcessDontNeedList(bool peek) TA_EXCL(lock_);

  // Helpers for adding and removing to the queues. All of the public Set/Move/Remove operations
  // are convenience wrappers around these.
  void RemoveLocked(vm_page_t* page, DeferPendingSignals& dps) TA_REQ(lock_);
  void SetQueueBacklinkLocked(vm_page_t* page, void* object, uintptr_t page_offset, PageQueue queue,
                              DeferPendingSignals& dps) TA_REQ(lock_);
  void MoveToQueueLocked(vm_page_t* page, PageQueue queue, DeferPendingSignals& dps) TA_REQ(lock_);

  // Updates the active/inactive counts assuming a single page has moved from |old_queue| to
  // |new_queue|. Either of these can be PageQueueNone to simulate pages being added or removed.
  void UpdateActiveInactiveLocked(PageQueue old_queue, PageQueue new_queue,
                                  DeferPendingSignals& dps) TA_REQ(lock_);

  // Recalculates |active_queue_count_| and |inactive_queue_count_|. This is pulled into a helper
  // method as this needs to be done both when accessed scanning completes, or if the mru_gen_ is
  // changed.
  void RecalculateActiveInactiveLocked(DeferPendingSignals& dps) TA_REQ(lock_);

  // Internal locked version of GetActiveInactiveCounts.
  ActiveInactiveCounts GetActiveInactiveCountsLocked() const TA_REQ(lock_);

  // Internal helper for shutting down any threads created in |StartThreads|.
  void StopThreads();

  // Entry point for the thread that will performing aging and increment the mru generation.
  void MruThread();

  // Checks if the active ratio has exceeded the threshold to cause aging, and if so signals the
  // event.
  void MaybeSignalActiveRatioAgingLocked(DeferPendingSignals& dps) TA_REQ(lock_);

  // Consumes any pending age reason and either returns the reason, or how long till aging will
  // happen. This timeout does not take into account that other changes, namely the active ratio,
  // could cause aging to be necessary before that timeout.
  // Due to the active ratio being sticky, it needs to be reset, which is why this method is called
  // consume. As a result, calling ConsumeAgeReason and then GetAgeReasonLocked could give different
  // results if an active ratio event was consumed and returned by the first call.
  ktl::variant<AgeReason, zx_instant_mono_t> ConsumeAgeReason() TA_EXCL(lock_);

  // Checks if there is any pending age reason that could be consumed. See ConsumeAgeReason for more
  // details.
  ktl::variant<AgeReason, zx_instant_mono_t> GetAgeReasonLocked() const TA_REQ(lock_);

  // Synchronizes with any outstanding aging. This is intended to allow a reclamation process to
  // ensure it is not racing with, and falsely failing to reclaim, the aging thread due to
  // scheduling or other delays.
  void SynchronizeWithAging() TA_EXCL(lock_);

  // Helper method that calculates whether the current active ratio would trigger aging.
  bool IsActiveRatioTriggeringAging() const TA_REQ(lock_);

  void LruThread();
  void MaybeTriggerLruProcessing() TA_EXCL(lock_);
  bool NeedsLruProcessingLocked() const TA_REQ(lock_);

  // MarkAccessed is split into a small inlinable portion that attempts to short circuit, and this
  // main implementation that does the actual accessed marking if needed.
  void MarkAccessedContinued(vm_page_t* page);

  // Returns true if a page is both in one of the Reclaim queues, and succeeds the passed in
  // validator, which takes a fbl::RefPtr<VmCowPages>.
  template <typename F>
  bool DebugPageIsSpecificReclaim(const vm_page_t* page, F validator, size_t* queue) const;

  // Returns true if a page is both in the specified |queue|, and succeeds the passed in validator,
  // which takes a fbl::RefPtr<VmCowPages>.
  template <typename F>
  bool DebugPageIsSpecificQueue(const vm_page_t* page, PageQueue queue, F validator) const;

  void AdvanceDontNeedCursorIf(vm_page_t* page) TA_REQ(lock_) {
    if (page == dont_need_cursor_) {
      dont_need_cursor_ = list_next_type(&page_queues_[PageQueueReclaimDontNeed], &page->queue_node,
                                         vm_page_t, queue_node);
    }
  }

  // The lock_ is needed to protect the linked lists queues as these cannot be implemented with
  // atomics.
  DECLARE_SPINLOCK(PageQueues) mutable lock_;

  // Declare a separate mutex for the aging event. This is a separate lock both because signalling
  // the event does not need to contend with other page queues operations, and so that it can be a
  // Mutex instead of a SpinLock allowing us to signal an event whilst holding said lock.
  DECLARE_CRITICAL_MUTEX(PageQueues) aging_event_mutex_;
  Event* aging_event_ TA_GUARDED(aging_event_mutex_) = nullptr;

  // This Event is a binary semaphore and is used to control aging. Is acquired by the aging thread
  // when it performs aging, and can be acquired separately to block aging. For this purpose it
  // needs to start as being initially signalled.
  PendingSignalEvent aging_token_{true};
  // Flag used to catch programming errors related to double enabling or disabling aging.
  ktl::atomic<bool> aging_disabled_ = false;

  // Time at which the mru_gen_ was last incremented.
  ktl::atomic<zx_instant_mono_t> last_age_time_ = ZX_TIME_INFINITE_PAST;
  // Reason the last aging event happened, this is purely for informational/debugging purposes.
  // Initialized to Timeout as a somewhat arbitrary choice.
  AgeReason last_age_reason_ TA_GUARDED(lock_) = AgeReason::Timeout;
  // Used to signal the aging thread that the active ratio has changed sufficiently that aging might
  // be required. Due to other factors, such as a min timeouts, races, etc, this being signaled does
  // not mean aging will happen.
  PendingSignalEvent aging_active_ratio_event_;
  // Tracks whether the active ratio has been tripped and should contribute as an aging trigger.
  // This is stored as a boolean so that it is sticky in the advent of a race with additional
  // modifications to the page queues. Were this not sticky then, in the absence of a debounce
  // threshold, we could repeatedly trigger the active ratio on and off, causing the aging thread
  // to repeatedly wake up, miss the trigger, and do nothing.
  bool active_ratio_triggered_ TA_GUARDED(lock_) = false;
  // Used to signal the lru thread that it should wake up and check if the lru queue needs
  // processing.
  PendingSignalEvent lru_event_;

  // Tracks whether there is a pending aging event that will happen that can be waited on. This is a
  // raw Event, and not an AutounsignalEvent, as it is a level triggered signal. The signal itself,
  // and hence any calls to Signal or Unsignal on the Event, must be coordinated by under the lock_
  // to ensure no races. Due to this need to coordinate a PendingSignalEvent cannot be used, and
  // preemption must be disabled to allow for manipulating the event with lock_ held.
  // This event itself gets set/cleared in both ConsumeAgeReason and SynchronizeWithAging based on
  // whether this is any aging reason present.
  Event no_pending_aging_signal_{true};

  // What to do with pages when processing the LRU queue.
  LruAction lru_action_ TA_GUARDED(lock_) = LruAction::None;

  // The page queues are placed into an array, indexed by page queue, for consistency and uniformity
  // of access. This does mean that the list for PageQueueNone does not actually have any pages in
  // it, and should always be empty.
  // The reclaimable queues are the more complicated as, unlike the other categories, pages can be
  // in one of the queues, and can move around. The reclaimable queues themselves store pages that
  // are roughly grouped by their last access time. The relationship is not precise as pages are not
  // moved between queues unless it becomes strictly necessary. This is in contrast to the queue
  // counts that are always up to date.
  //
  // What this means is that the vm_page::page_queue index is always up to do date, and the
  // page_queue_counts_ represent an accurate count of pages with that vm_page::page_queue index,
  // but counting the pages actually in the linked list may not yield the correct number.
  //
  // New reclaimable pages are always placed into the queue associated with the MRU generation. If
  // they get accessed the vm_page_t::page_queue gets updated along with the counts. At some point
  // the LRU queue will get processed (see |ProcessDontNeedAndLruQueues|) and this will cause pages
  // to get relocated to their correct list.
  //
  // Consider the following example:
  //
  //  LRU  MRU            LRU  MRU            LRU   MRU            LRU   MRU        MRU  LRU
  //    |  |                |  |                |     |              |     |            |  |
  //    |  |    Insert A    |  |    Age         |     |  Touch A     |     |  Age       |  |
  //    V  v    Queue=2     v  v    Queue=2     v     v  Queue=3     v     v  Queue=3   v  v
  // [][ ][ ][] -------> [][ ][a][] -------> [][ ][a][ ] -------> [][ ][a][ ] -------> [ ][ ][a][]
  //
  // At this point page A, in its vm_page_t, has its queue marked as 3, and the page_queue_counts
  // are {0,0,1,0}, but the page itself remains in the linked list for queue 2. If the LRU queue is
  // then processed to increment it we would do.
  //
  //  MRU  LRU             MRU  LRU            MRU    LRU
  //    |  |                 |    |              |      |
  //    |  |       Move LRU  |    |    Move LRU  |      |
  //    V  v       Queue=3   v    v    Queue=3   v      v
  //   [ ][ ][a][] -------> [ ][][a][] -------> [][ ][][a]
  //
  // In the second processing of the LRU queue it gets noticed that the page, based on
  // vm_page_t::page_queue, is in the wrong queue and gets moved into the correct one.
  //
  // For specifics on how LRU and MRU generations map to LRU and MRU queues, see comments on
  // |lru_gen_| and |mru_gen_|.
  ktl::array<list_node_t, PageQueueNumQueues> page_queues_ TA_GUARDED(lock_);

  // The generation counts are monotonic increasing counters and used to represent the effective age
  // of the oldest and newest reclaimable queues. The page queues themselves are treated as a fixed
  // size circular buffer that the generations map onto (see definition of |gen_to_queue|).This
  // means all pages in the system have an age somewhere in [lru_gen_, mru_gen_] and so the lru and
  // mru generations cannot drift apart by more than kNumPagerBacked, otherwise there would not be
  // enough queues.
  // A pages age being between [lru_gen_, mru_gen_] is not an invariant as MarkAccessed can race and
  // mark pages as being in an invalid queue. This race will get noticed by ProcessLruQueues and
  // the page will get updated at that point to have a valid queue. Importantly, whilst pages can
  // think they are in a queue that is invalid, only valid linked lists in the page_queues_ will
  // ever have pages in them. This invariant is easy to enforce as the page_queues_ are updated
  // under a lock.
  // These are atomic so they can be safely read without the lock held, however they are always
  // modified with the lock hold.
  ktl::atomic<uint64_t> lru_gen_ = 0;
  ktl::atomic<uint64_t> mru_gen_ = kNumReclaim - 1;

  // This semaphore counts the amount of space remaining for the mru to grow before it would overlap
  // with the lru. Having this as a semaphore (even though it can always be calculated from lru_gen_
  // and mru_gen_ above) provides a way for the aging thread to block when it needs to wait for
  // eviction/lru processing to happen. This allows eviction/lru processing to be happening
  // concurrently in a different thread, without requiring it to happen in-line in the aging thread.
  // Without this the aging thread would need to process the LRU queue directly if it needed to make
  // space. Initially, with the lru_gen_ and mru_gen_ definitions above, we start with no space for
  // the mru to grow, so initialize this to 0.
  Semaphore mru_semaphore_ = Semaphore(0);

  // Tracks the counts of pages in each queue in O(1) time complexity. As pages are moved between
  // queues, the corresponding source and destination counts are decremented and incremented,
  // respectively.
  //
  // The first entry of the array is left special: it logically represents pages not in any queue.
  // For simplicity, it is initialized to zero rather than the total number of pages in the system.
  // Consequently, the value of this entry is a negative number with absolute value equal to the
  // total number of pages in all queues. This approach avoids unnecessary branches when updating
  // counts.
  ktl::array<ktl::atomic<size_t>, PageQueueNumQueues> page_queue_counts_ = {};

  // These are the continuously updated active/inactive queue counts. Continuous here means updated
  // by all page queue methods except for MarkAccessedDeferredCount. Due to races whilst accessed
  // harvesting is happening, these could be inaccurate or even become negative, and should not be
  // read from whilst used_cached_queue_counts_ is true, and need to be completely recalculated
  // prior to setting |used_cached_queue_counts_| back to false.
  int64_t active_queue_count_ TA_GUARDED(lock_) = 0;
  int64_t inactive_queue_count_ TA_GUARDED(lock_) = 0;
  // When accessed harvesting is happening these hold the last known 'good' values of the
  // active/inactive queue counts.
  uint64_t cached_active_queue_count_ TA_GUARDED(lock_) = 0;
  uint64_t cached_inactive_queue_count_ TA_GUARDED(lock_) = 0;
  // Indicates whether the cached counts should be returned in queries or not. This also indicates
  // whether the page queues expect accessed harvesting to be happening. This is only an atomic
  // so that MarkAccessedDeferredCount can reference it in a DEBUG_ASSERT without triggering
  // memory safety issues.
  ktl::atomic<bool> use_cached_queue_counts_ = false;

  // Track the mru and lru threads and have a signalling mechanism to shut them down.
  ktl::atomic<bool> shutdown_threads_ = false;
  Thread* mru_thread_ TA_GUARDED(lock_) = nullptr;
  Thread* lru_thread_ TA_GUARDED(lock_) = nullptr;

  // Debug compressor is only available when debug asserts are also enabled. This ensures it can
  // never have an impact on production builds.
#if DEBUG_ASSERT_IMPLEMENTED
  ktl::unique_ptr<VmDebugCompressor> debug_compressor_ TA_GUARDED(lock_);
#endif

  // Queue rotation parameters. These are not locked as they are only read by the mru thread, and
  // are set before the mru thread is started.
  zx_duration_mono_t min_mru_rotate_time_;
  zx_duration_mono_t max_mru_rotate_time_;

  // Determines if anonymous zero page forks are placed in the zero fork queue or in the reclaimable
  // queue.
  RelaxedAtomic<bool> zero_fork_is_reclaimable_ = false;

  // Determines if anonymous pages are placed in the reclaimable queues, or in their own non aging
  // anonymous queues.
  RelaxedAtomic<bool> anonymous_is_reclaimable_ = false;

  // Current active ratio multiplier.
  uint64_t active_ratio_multiplier_ TA_GUARDED(lock_);

  // In order to process all the items in the DontNeed list, whilst still being able to periodically
  // drop the lock to avoid a long running operation, we use a cursor to record the current page
  // being examined. Any operation that removes a page from the DontNeed list must check if its
  // removing this page, and advance it to the next page in the list if it is. This is automated by
  // calling AdvanceDontNeedCursorIf.
  vm_page_t* dont_need_cursor_ TA_GUARDED(lock_) = nullptr;

  // There is only a single cursor and this lock acts as a resource control for it. This process
  // needs to be done on LRU rotation / when peeking pages for reclamation, which need to be
  // synchronized anyway so having this additional lock does not impact parallelism.
  // The actual object member |dont_need_cursor_| is not guarded by this lock, since it needs to be
  // read, and potentially updated, specifically by other threads who do not own the logical cursor.
  DECLARE_MUTEX(PageQueues) dont_need_cursor_lock_;
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_H_
