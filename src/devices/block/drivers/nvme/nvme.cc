// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/nvme/nvme.h"

#include <lib/fit/defer.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/mmio/mmio-buffer.h>
#include <lib/sync/completion.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <threads.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/threads.h>
#include <zircon/types.h>

#include <mutex>

#include <hwreg/bitfields.h>

#include "src/devices/block/drivers/nvme/commands/features.h"
#include "src/devices/block/drivers/nvme/commands/identify.h"
#include "src/devices/block/drivers/nvme/commands/nvme-io.h"
#include "src/devices/block/drivers/nvme/commands/queue.h"
#include "src/devices/block/drivers/nvme/namespace.h"
#include "src/devices/block/drivers/nvme/registers.h"

namespace nvme {

// For safety - so that the driver doesn't draw too many resources.
constexpr size_t kMaxNamespacesToBind = 4;

// c.f. NVMe Base Specification 2.0, section 3.1.3.8 "AQA - Admin Queue Attributes"
constexpr size_t kAdminQueueMaxEntries = 4096;

// TODO(https://fxbug.dev/42053036): Consider using interrupt vector - dedicated interrupt (and IO
// thread) per namespace/queue.
int Nvme::IrqLoop() {
  while (true) {
    zx_status_t status = irq_.wait(nullptr);
    if (status != ZX_OK) {
      if (status == ZX_ERR_CANCELED) {
        FDF_LOG(DEBUG, "Interrupt cancelled. Exiting IRQ loop.");
      } else {
        FDF_LOG(ERROR, "Failed to wait for interrupt: %s", zx_status_get_string(status));
      }
      break;
    }

    // The interrupt mask register should only be used when not using MSI-X.
    if (irq_mode_ != fuchsia_hardware_pci::InterruptMode::kMsiX) {
      InterruptReg::MaskSet().FromValue(1).WriteTo(&*mmio_);
    }

    sync_completion_signal(&io_signal_);

    if (irq_mode_ != fuchsia_hardware_pci::InterruptMode::kMsiX) {
      // Unmask the interrupt.
      InterruptReg::MaskClear().FromValue(1).WriteTo(&*mmio_);
    }

    if (irq_mode_ == fuchsia_hardware_pci::InterruptMode::kLegacy) {
      status = pci_.AckInterrupt();
      if (status != ZX_OK) {
        FDF_LOG(ERROR, "Failed to ack interrupt: %s", zx_status_get_string(status));
        break;
      }
    }
  }
  return thrd_success;
}

zx::result<Completion> Nvme::DoAdminCommandSync(Submission& submission,
                                                std::optional<zx::unowned_vmo> admin_data) {
  zx_status_t status;
  std::lock_guard<std::mutex> lock(admin_lock_);

  uint64_t data_size = 0;
  if (admin_data.has_value()) {
    status = admin_data.value()->get_size(&data_size);
    if (status != ZX_OK) {
      FDF_LOG(ERROR, "Failed to get size of vmo: %s", zx_status_get_string(status));
      return zx::error(status);
    }
  }
  status = admin_queue_->Submit(submission, admin_data, 0, data_size, nullptr);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to submit admin command: %s", zx_status_get_string(status));
    return zx::error(status);
  }

  zx::duration total_wait = zx::msec(0);
  while (true) {
    Completion* completion;
    status = admin_queue_->CheckForNewCompletion(&completion);
    if (status == ZX_ERR_SHOULD_WAIT) {
      if (total_wait >= zx::sec(10)) {  // Wait for up to 10 seconds for an admin command.
        FDF_LOG(ERROR, "Timed out waiting for admin command: %s", zx_status_get_string(status));
        return zx::error(status);
      }

      // Wait and check for command completion again.
      const zx::duration incremental_wait = zx::msec(1);
      zx::nanosleep(zx::deadline_after(incremental_wait));
      total_wait += incremental_wait;
      continue;
    }

    auto ring_completion_doorbell = fit::defer([&] { admin_queue_->RingCompletionDb(); });

    if (status != ZX_OK) {
      FDF_LOG(ERROR, "Failed to check completion of admin command: %s",
              zx_status_get_string(status));
      return zx::error(status);
    }

    if (completion->status_code_type() == StatusCodeType::kGeneric &&
        completion->status_code() == 0) {
      FDF_LOG(TRACE, "Completed admin command OK.");
    } else {
      FDF_LOG(ERROR, "Completed admin command ERROR: status type=%01x, status=%02x",
              completion->status_code_type(), completion->status_code());
      return zx::error(ZX_ERR_IO);
    }

    return zx::ok(*completion);
  }
}

void Nvme::ProcessIoSubmissions() {
  while (true) {
    IoCommand* io_cmd;
    {
      std::lock_guard<std::mutex> lock(commands_lock_);
      io_cmd = list_remove_head_type(&pending_commands_, IoCommand, node);
    }

    if (io_cmd == nullptr) {
      return;
    }

    zx_status_t status;
    const auto opcode = io_cmd->op.command.opcode;
    if (opcode == BLOCK_OPCODE_FLUSH) {
      NvmIoFlushSubmission submission;
      submission.namespace_id = io_cmd->namespace_id;

      status = io_queue_->Submit(submission, std::nullopt, 0, 0, io_cmd);
    } else {
      NvmIoSubmission submission(opcode == BLOCK_OPCODE_WRITE);
      submission.namespace_id = io_cmd->namespace_id;
      submission.set_start_lba(io_cmd->op.rw.offset_dev).set_block_count(io_cmd->op.rw.length - 1);
      if (io_cmd->op.command.flags & BLOCK_IO_FLAG_FORCE_ACCESS) {
        submission.set_force_unit_access(true);
      }

      // Convert op.rw.offset_vmo and op.rw.length to bytes.
      status = io_queue_->Submit(submission, zx::unowned_vmo(io_cmd->op.rw.vmo),
                                 io_cmd->op.rw.offset_vmo * io_cmd->block_size_bytes,
                                 io_cmd->op.rw.length * io_cmd->block_size_bytes, io_cmd);
    }
    switch (status) {
      case ZX_OK:
        break;
      case ZX_ERR_SHOULD_WAIT:
        // We can't proceed if there is no available space in the submission queue. Put command back
        // at front of queue for further processing later.
        {
          std::lock_guard<std::mutex> lock(commands_lock_);
          list_add_head(&pending_commands_, &io_cmd->node);
        }
        return;
      default:
        FDF_LOG(ERROR, "Failed to submit transaction (command %p): %s", io_cmd,
                zx_status_get_string(status));
        io_cmd->Complete(ZX_ERR_INTERNAL);
        break;
    }
  }
}

void Nvme::ProcessIoCompletions() {
  bool ring_doorbell = false;
  Completion* completion = nullptr;
  IoCommand* io_cmd = nullptr;
  while (io_queue_->CheckForNewCompletion(&completion, &io_cmd) != ZX_ERR_SHOULD_WAIT) {
    ring_doorbell = true;

    if (io_cmd == nullptr) {
      FDF_LOG(ERROR, "Completed transaction isn't associated with a command.");
      continue;
    }

    if (completion->status_code_type() == StatusCodeType::kGeneric &&
        completion->status_code() == 0) {
      FDF_LOG(TRACE, "Completed transaction #%u command %p OK.", completion->command_id(), io_cmd);
      io_cmd->Complete(ZX_OK);
    } else {
      FDF_LOG(ERROR, "Completed transaction #%u command %p ERROR: status type=%01x, status=%02x",
              completion->command_id(), io_cmd, completion->status_code_type(),
              completion->status_code());
      io_cmd->Complete(ZX_ERR_IO);
    }
  }

  if (ring_doorbell) {
    io_queue_->RingCompletionDb();
  }
}

int Nvme::IoLoop() {
  while (true) {
    if (driver_shutdown_) {  // Check this outside of io_signal_ wait-reset below to avoid deadlock.
      FDF_LOG(DEBUG, "IO thread exiting.");
      break;
    }

    zx_status_t status = sync_completion_wait(&io_signal_, ZX_TIME_INFINITE);
    if (status != ZX_OK) {
      FDF_LOG(ERROR, "Failed to wait for sync completion: %s", zx_status_get_string(status));
      break;
    }
    sync_completion_reset(&io_signal_);

    // process completion messages
    ProcessIoCompletions();

    // process work queue
    ProcessIoSubmissions();
  }
  return thrd_success;
}

void Nvme::QueueIoCommand(IoCommand* io_cmd) {
  {
    std::lock_guard<std::mutex> lock(commands_lock_);
    list_add_tail(&pending_commands_, &io_cmd->node);
  }

  sync_completion_signal(&io_signal_);
}

void Nvme::PrepareStop(fdf::PrepareStopCompleter completer) {
  FDF_LOG(DEBUG, "Preparing to stop driver.");
  driver_shutdown_ = true;
  if (pci_.is_valid()) {
    pci_.SetBusMastering(false);
  }
  irq_.destroy();  // Make irq_.wait() in IrqLoop() return ZX_ERR_CANCELED.
  if (irq_thread_started_) {
    thrd_join(irq_thread_, nullptr);
  }
  if (io_thread_started_) {
    sync_completion_signal(&io_signal_);
    thrd_join(io_thread_, nullptr);
  }

  // Error out any pending commands
  {
    std::lock_guard<std::mutex> lock(commands_lock_);
    IoCommand* io_cmd;
    while ((io_cmd = list_remove_head_type(&pending_commands_, IoCommand, node)) != nullptr) {
      io_cmd->Complete(ZX_ERR_PEER_CLOSED);
    }
  }

  completer(zx::ok());
}

static zx_status_t WaitForReset(bool desired_ready_state, fdf::MmioBuffer* mmio) {
  constexpr int kResetWaitMs = 5000;
  int ms_remaining = kResetWaitMs;
  while (ControllerStatusReg::Get().ReadFrom(mmio).ready() != desired_ready_state) {
    if (ms_remaining-- == 0) {
      FDF_LOG(ERROR, "Timed out waiting for controller ready state %u: ", desired_ready_state);
      return ZX_ERR_TIMED_OUT;
    }
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  FDF_LOG(DEBUG, "Controller reached ready state %u (took %u ms).", desired_ready_state,
          kResetWaitMs - ms_remaining);
  return ZX_OK;
}

static zx_status_t CheckMinMaxSize(const std::string& name, size_t our_size, size_t min_size,
                                   size_t max_size) {
  if (our_size < min_size) {
    FDF_LOG(ERROR, "%s size is too small (ours: %zu, min: %zu).", name.c_str(), our_size, min_size);
    return ZX_ERR_NOT_SUPPORTED;
  }
  if (our_size > max_size) {
    FDF_LOG(ERROR, "%s size is too large (ours: %zu, max: %zu).", name.c_str(), our_size, max_size);
    return ZX_ERR_NOT_SUPPORTED;
  }
  return ZX_OK;
}

static void PopulateVersionInspect(const VersionReg& version_reg, inspect::Node* inspect_node,
                                   inspect::Inspector* inspector) {
  auto version = inspect_node->CreateChild("version");
  version.RecordInt("major", version_reg.major());
  version.RecordInt("minor", version_reg.minor());
  version.RecordInt("tertiary", version_reg.tertiary());
  inspector->emplace(std::move(version));
}

static void PopulateCapabilitiesInspect(const CapabilityReg& caps_reg,
                                        const VersionReg& version_reg, inspect::Node* inspect_node,
                                        inspect::Inspector* inspector) {
  auto caps = inspect_node->CreateChild("capabilities");
  if (version_reg >= VersionReg::FromVer(1, 4, 0)) {
    caps.RecordBool("controller_ready_independent_media_supported",
                    caps_reg.controller_ready_independent_media_supported());
    caps.RecordBool("controller_ready_with_media_supported",
                    caps_reg.controller_ready_with_media_supported());
  }
  caps.RecordBool("subsystem_shutdown_supported", caps_reg.subsystem_shutdown_supported());
  caps.RecordBool("controller_memory_buffer_supported",
                  caps_reg.controller_memory_buffer_supported());
  caps.RecordBool("persistent_memory_region_supported",
                  caps_reg.persistent_memory_region_supported());
  caps.RecordInt("memory_page_size_max_bytes", caps_reg.memory_page_size_max_bytes());
  caps.RecordInt("memory_page_size_min_bytes", caps_reg.memory_page_size_min_bytes());
  caps.RecordInt("controller_power_scope", caps_reg.controller_power_scope());
  caps.RecordBool("boot_partition_support", caps_reg.boot_partition_support());
  caps.RecordBool("no_io_command_set_support", caps_reg.no_io_command_set_support());
  caps.RecordBool("identify_io_command_set_support", caps_reg.identify_io_command_set_support());
  caps.RecordBool("nvm_command_set_support", caps_reg.nvm_command_set_support());
  caps.RecordBool("nvm_subsystem_reset_supported", caps_reg.nvm_subsystem_reset_supported());
  caps.RecordInt("doorbell_stride_bytes", caps_reg.doorbell_stride_bytes());
  // TODO(https://fxbug.dev/42053036): interpret CRTO register if version > 1.4
  caps.RecordInt("ready_timeout_ms", caps_reg.timeout_ms());
  caps.RecordBool("vendor_specific_arbitration_supported",
                  caps_reg.vendor_specific_arbitration_supported());
  caps.RecordBool("weighted_round_robin_arbitration_supported",
                  caps_reg.weighted_round_robin_arbitration_supported());
  caps.RecordBool("contiguous_queues_required", caps_reg.contiguous_queues_required());
  caps.RecordInt("max_queue_entries", caps_reg.max_queue_entries());
  inspector->emplace(std::move(caps));
}

static void PopulateControllerInspect(const IdentifyController& identify,
                                      uint32_t max_data_transfer_bytes,
                                      uint16_t atomic_write_unit_normal,
                                      uint16_t atomic_write_unit_power_fail,
                                      bool volatile_write_cache_present,
                                      bool volatile_write_cache_enabled,
                                      inspect::Node* inspect_node, inspect::Inspector* inspector) {
  auto controller = inspect_node->CreateChild("controller");
  auto model_number = std::string(identify.model_number, sizeof(identify.model_number));
  auto serial_number = std::string(identify.serial_number, sizeof(identify.serial_number));
  auto firmware_rev = std::string(identify.firmware_rev, sizeof(identify.firmware_rev));
  // Some vendors don't pad the strings with spaces (0x20). Null-terminate strings to avoid printing
  // illegal characters.
  model_number = std::string(model_number.c_str());
  serial_number = std::string(serial_number.c_str());
  firmware_rev = std::string(firmware_rev.c_str());
  FDF_LOG(INFO, "Model number:  '%s'", model_number.c_str());
  FDF_LOG(INFO, "Serial number: '%s'", serial_number.c_str());
  FDF_LOG(INFO, "Firmware rev.: '%s'", firmware_rev.c_str());
  controller.RecordString("model_number", model_number);
  controller.RecordString("serial_number", serial_number);
  controller.RecordString("firmware_rev", firmware_rev);
  controller.RecordInt("max_outstanding_commands", identify.max_cmd);
  controller.RecordInt("num_namespaces", identify.num_namespaces);
  controller.RecordInt("max_allowed_namespaces", identify.max_allowed_namespaces);
  controller.RecordBool("sgl_support", identify.sgl_support & 3);
  controller.RecordInt("max_data_transfer_bytes", max_data_transfer_bytes);
  controller.RecordInt("sanitize_caps", identify.sanicap & 3);
  controller.RecordInt("abort_command_limit", identify.acl + 1);
  controller.RecordInt("asynch_event_req_limit", identify.aerl + 1);
  controller.RecordInt("firmware_slots", (identify.frmw >> 1) & 3);
  controller.RecordBool("firmware_reset", !(identify.frmw & (1 << 4)));
  controller.RecordBool("firmware_slot1ro", identify.frmw & 1);
  controller.RecordInt("host_buffer_min_pages", identify.hmmin);
  controller.RecordInt("host_buffer_preferred_pages", identify.hmpre);
  controller.RecordInt("capacity_total", identify.tnvmcap[0]);
  controller.RecordInt("capacity_unalloc", identify.unvmcap[0]);
  controller.RecordBool("volatile_write_cache_present", volatile_write_cache_present);
  controller.RecordBool("volatile_write_cache_enabled", volatile_write_cache_enabled);
  controller.RecordInt("atomic_write_unit_normal_blocks", atomic_write_unit_normal);
  controller.RecordInt("atomic_write_unit_power_fail_blocks", atomic_write_unit_power_fail);
  controller.RecordBool("doorbell_buffer_config", identify.doorbell_buffer_config());
  controller.RecordBool("virtualization_management", identify.virtualization_management());
  controller.RecordBool("nvme_mi_send_recv", identify.nvme_mi_send_recv());
  controller.RecordBool("directive_send_recv", identify.directive_send_recv());
  controller.RecordBool("device_self_test", identify.device_self_test());
  controller.RecordBool("namespace_management", identify.namespace_management());
  controller.RecordBool("firmware_download_commit", identify.firmware_download_commit());
  controller.RecordBool("format_nvm", identify.format_nvm());
  controller.RecordBool("security_send_recv", identify.security_send_recv());
  controller.RecordBool("timestamp", identify.timestamp());
  controller.RecordBool("reservations", identify.reservations());
  controller.RecordBool("save_select_nonzero", identify.save_select_nonzero());
  controller.RecordBool("write_uncorrectable", identify.write_uncorrectable());
  controller.RecordBool("compare", identify.compare());
  inspector->emplace(std::move(controller));
}

zx_status_t Nvme::Init() {
  list_initialize(&pending_commands_);

  VersionReg version_reg = VersionReg::Get().ReadFrom(&*mmio_);
  CapabilityReg caps_reg = CapabilityReg::Get().ReadFrom(&*mmio_);

  inspect_node_ = inspector().root().CreateChild("nvme");
  PopulateVersionInspect(version_reg, &inspect_node_, &inspect());
  PopulateCapabilitiesInspect(caps_reg, version_reg, &inspect_node_, &inspect());

  const size_t kPageSize = zx_system_get_page_size();
  zx_status_t status =
      CheckMinMaxSize("System page", kPageSize, caps_reg.memory_page_size_min_bytes(),
                      caps_reg.memory_page_size_max_bytes());
  if (status != ZX_OK) {
    return status;
  }

  if (ControllerStatusReg::Get().ReadFrom(&*mmio_).ready()) {
    FDF_LOG(DEBUG, "Controller is already enabled. Resetting it.");
    ControllerConfigReg::Get().ReadFrom(&*mmio_).set_enabled(0).WriteTo(&*mmio_);
    status = WaitForReset(/*desired_ready_state=*/false, &*mmio_);
    if (status != ZX_OK) {
      return status;
    }
  }

  // Set up admin submission and completion queues.
  zx::result admin_queue =
      QueuePair::Create(bti_.borrow(), 0, kAdminQueueMaxEntries, caps_reg, *mmio_,
                        /*prealloc_prp=*/false);
  if (admin_queue.is_error()) {
    FDF_LOG(ERROR, "Failed to set up admin queue: %s", admin_queue.status_string());
    return admin_queue.status_value();
  }
  admin_queue_ = std::move(*admin_queue);

  // Configure the admin queue.
  AdminQueueAttributesReg::Get()
      .ReadFrom(&*mmio_)
      .set_completion_queue_size(admin_queue_->completion().entry_count() - 1)
      .set_submission_queue_size(admin_queue_->submission().entry_count() - 1)
      .WriteTo(&*mmio_);

  AdminQueueAddressReg::CompletionQueue()
      .ReadFrom(&*mmio_)
      .set_addr(admin_queue_->completion().GetDeviceAddress())
      .WriteTo(&*mmio_);
  AdminQueueAddressReg::SubmissionQueue()
      .ReadFrom(&*mmio_)
      .set_addr(admin_queue_->submission().GetDeviceAddress())
      .WriteTo(&*mmio_);

  FDF_LOG(DEBUG, "Enabling controller.");
  ControllerConfigReg::Get()
      .ReadFrom(&*mmio_)
      .set_controller_ready_independent_of_media(0)
      // Queue entry sizes are powers of two.
      .set_io_completion_queue_entry_size(__builtin_ctzl(sizeof(Completion)))
      .set_io_submission_queue_entry_size(__builtin_ctzl(sizeof(Submission)))
      .set_arbitration_mechanism(ControllerConfigReg::ArbitrationMechanism::kRoundRobin)
      // We know that page size is always at least 4096 (required by spec), and we check
      // that zx_system_get_page_size is supported by the controller above.
      .set_memory_page_size(__builtin_ctzl(kPageSize) - 12)
      .set_io_command_set(ControllerConfigReg::CommandSet::kNvm)
      .set_enabled(1)
      .WriteTo(&*mmio_);

  status = WaitForReset(/*desired_ready_state=*/true, &*mmio_);
  if (status != ZX_OK) {
    return status;
  }

  // Timeout may have changed, so double check it.
  caps_reg.ReadFrom(&*mmio_);

  // Set up IO submission and completion queues.
  zx::result io_queue =
      QueuePair::Create(bti_.borrow(), 1, caps_reg.max_queue_entries(), caps_reg, *mmio_,
                        /*prealloc_prp=*/true);
  if (io_queue.is_error()) {
    FDF_LOG(ERROR, "Failed to set up io queue: %s", io_queue.status_string());
    return io_queue.status_value();
  }
  io_queue_ = std::move(*io_queue);
  inspect_node_.RecordInt("io_submission_queue_size", io_queue_->submission().entry_count());
  inspect_node_.RecordInt("io_completion_queue_size", io_queue_->completion().entry_count());

  // TODO: Switch to use dispatcher threads instead.
  // Spin up IRQ thread so we can start issuing admin commands to the device.
  int thrd_status = thrd_create_with_name(
      &irq_thread_, [](void* ctx) { return static_cast<Nvme*>(ctx)->IrqLoop(); }, this,
      "nvme-irq-thread");
  if (thrd_status != thrd_success) {
    status = thrd_status_to_zx_status(thrd_status);
    FDF_LOG(ERROR, " Failed to create IRQ thread: %s", zx_status_get_string(status));
    return status;
  }
  irq_thread_started_ = true;

  // TODO: Switch to use dispatcher threads instead.
  // Spin up IO thread so we can start issuing IO commands from namespace(s).
  thrd_status = thrd_create_with_name(
      &io_thread_, [](void* ctx) { return static_cast<Nvme*>(ctx)->IoLoop(); }, this,
      "nvme-io-thread");
  if (thrd_status != thrd_success) {
    status = thrd_status_to_zx_status(thrd_status);
    FDF_LOG(ERROR, " Failed to create IO thread: %s", zx_status_get_string(status));
    return status;
  }
  io_thread_started_ = true;

  zx::vmo admin_data;
  status = zx::vmo::create(kPageSize, 0, &admin_data);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to create vmo: %s", zx_status_get_string(status));
    return status;
  }

  fzl::VmoMapper mapper;
  status = mapper.Map(admin_data);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to map vmo: %s", zx_status_get_string(status));
    return status;
  }

  IdentifySubmission identify_controller;
  identify_controller.set_structure(IdentifySubmission::IdentifyCns::kIdentifyController);
  zx::result<Completion> completion = DoAdminCommandSync(identify_controller, admin_data.borrow());
  if (completion.is_error()) {
    FDF_LOG(ERROR, "Failed to identify controller: %s", completion.status_string());
    return completion.status_value();
  }

  auto identify = static_cast<IdentifyController*>(mapper.start());

  status = CheckMinMaxSize("Submission queue entry", sizeof(Submission),
                           identify->minimum_sq_entry_size(), identify->maximum_sq_entry_size());
  if (status != ZX_OK) {
    return status;
  }
  status = CheckMinMaxSize("Completion queue entry", sizeof(Completion),
                           identify->minimum_cq_entry_size(), identify->maximum_cq_entry_size());
  if (status != ZX_OK) {
    return status;
  }

  if (identify->max_data_transfer == 0) {
    max_data_transfer_bytes_ = 0;
  } else {
    max_data_transfer_bytes_ =
        caps_reg.memory_page_size_min_bytes() * (1 << identify->max_data_transfer);
  }
  atomic_write_unit_normal_ = identify->atomic_write_unit_normal + 1;
  atomic_write_unit_power_fail_ = identify->atomic_write_unit_power_fail + 1;

  bool volatile_write_cache_present = identify->vwc & 1;
  if (volatile_write_cache_present) {
    // Get 'Volatile Write Cache Enable' feature.
    GetVolatileWriteCacheSubmission get_vwc_enable;
    completion = DoAdminCommandSync(get_vwc_enable);
    if (completion.is_error()) {
      FDF_LOG(ERROR, "Failed to get 'Volatile Write Cache' feature: %s",
              completion.status_string());
      return completion.status_value();
    } else {
      const auto& vwc_completion = completion->GetCompletion<GetVolatileWriteCacheCompletion>();
      volatile_write_cache_enabled_ = vwc_completion.get_volatile_write_cache_enabled();
      FDF_LOG(DEBUG, "Volatile write cache is %s",
              volatile_write_cache_enabled_ ? "enabled" : "disabled");
    }
  }

  PopulateControllerInspect(*identify, max_data_transfer_bytes_, atomic_write_unit_normal_,
                            atomic_write_unit_power_fail_, volatile_write_cache_present,
                            volatile_write_cache_enabled_, &inspect_node_, &inspect());

  // Set feature (number of queues) to 1 IO submission queue and 1 IO completion queue.
  SetIoQueueCountSubmission set_queue_count;
  set_queue_count.set_num_submission_queues(1).set_num_completion_queues(1);
  completion = DoAdminCommandSync(set_queue_count);
  if (completion.is_error()) {
    FDF_LOG(ERROR, "Failed to set feature (number of queues): %s", completion.status_string());
    return completion.status_value();
  }
  const auto& ioq_completion = completion->GetCompletion<SetIoQueueCountCompletion>();
  if (ioq_completion.num_submission_queues() < 1) {
    FDF_LOG(ERROR, "Unexpected IO submission queue count: %u",
            ioq_completion.num_submission_queues());
    return ZX_ERR_IO;
  }
  if (ioq_completion.num_completion_queues() < 1) {
    FDF_LOG(ERROR, "Unexpected IO completion queue count: %u",
            ioq_completion.num_completion_queues());
    return ZX_ERR_IO;
  }

  // Create IO completion queue.
  CreateIoCompletionQueueSubmission create_iocq;
  create_iocq.set_queue_id(io_queue_->completion().id())
      .set_queue_size(io_queue_->completion().entry_count() - 1)
      .set_contiguous(true)
      .set_interrupt_en(true)
      .set_interrupt_vector(0);
  create_iocq.data_pointer[0] = io_queue_->completion().GetDeviceAddress();
  completion = DoAdminCommandSync(create_iocq);
  if (completion.is_error()) {
    FDF_LOG(ERROR, "Failed to create IO completion queue: %s", completion.status_string());
    return completion.status_value();
  }

  // Create IO submission queue.
  CreateIoSubmissionQueueSubmission create_iosq;
  create_iosq.set_queue_id(io_queue_->submission().id())
      .set_queue_size(io_queue_->submission().entry_count() - 1)
      .set_completion_queue_id(io_queue_->completion().id())
      .set_contiguous(true);
  create_iosq.data_pointer[0] = io_queue_->submission().GetDeviceAddress();
  completion = DoAdminCommandSync(create_iosq);
  if (completion.is_error()) {
    FDF_LOG(ERROR, "Failed to create IO submission queue: %s", completion.status_string());
    return completion.status_value();
  }

  // Identify active namespaces.
  IdentifySubmission identify_ns_list;
  identify_ns_list.set_structure(IdentifySubmission::IdentifyCns::kActiveNamespaceList);
  completion = DoAdminCommandSync(identify_ns_list, admin_data.borrow());
  if (completion.is_error()) {
    FDF_LOG(ERROR, "Failed to identify active namespace list: %s", completion.status_string());
    return completion.status_value();
  }

  // Bind active namespaces.
  auto ns_list = static_cast<IdentifyActiveNamespaces*>(mapper.start());
  for (size_t i = 0; i < std::size(ns_list->nsid) && ns_list->nsid[i] != 0; i++) {
    if (i >= kMaxNamespacesToBind) {
      FDF_LOG(WARNING, "Skipping additional namespaces after adding %zu.", i);
      break;
    }
    const uint32_t namespace_id = ns_list->nsid[i];
    zx::result<std::unique_ptr<Namespace>> ns = Namespace::Bind(this, namespace_id);
    if (ns.is_error()) {
      FDF_LOG(ERROR, "Failed to add namespace %u: %s", namespace_id, ns.status_string());
      return ns.status_value();
    }
    namespaces_.push_back(*std::move(ns));
  }

  return ZX_OK;
}

zx::result<fit::function<void()>> Nvme::InitResources() {
  zx::result<fidl::ClientEnd<fuchsia_hardware_pci::Device>> pci_client_result =
      incoming()->Connect<fuchsia_hardware_pci::Service::Device>();
  if (pci_client_result.is_error()) {
    FDF_LOG(ERROR, "Failed to get pci client: %s", pci_client_result.status_string());
    return pci_client_result.take_error();
  }

  pci_ = ddk::Pci(std::move(pci_client_result).value());
  if (!pci_.is_valid()) {
    FDF_LOG(ERROR, "Failed to find PCI fragment");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  std::optional<fdf::MmioBuffer> mmio;
  zx_status_t status = pci_.MapMmio(0u, ZX_CACHE_POLICY_UNCACHED_DEVICE, &mmio);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to map registers: %s", zx_status_get_string(status));
    return zx::error(status);
  }
  mmio_ = std::move(mmio);

  status = pci_.ConfigureInterruptMode(1, &irq_mode_);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to configure interrupt: %s", zx_status_get_string(status));
    return zx::error(status);
  }
  FDF_LOG(DEBUG, "Interrupt mode: %u", static_cast<uint8_t>(irq_mode_));

  status = pci_.MapInterrupt(0, &irq_);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to map interrupt: %s", zx_status_get_string(status));
    return zx::error(status);
  }

  status = pci_.SetBusMastering(true);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to enable bus mastering: %s", zx_status_get_string(status));
    return zx::error(status);
  }
  auto release = [this] { pci_.SetBusMastering(false); };

  status = pci_.GetBti(0, &bti_);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to get BTI handle: %s", zx_status_get_string(status));
    return zx::error(status);
  }
  return zx::ok(std::move(release));
}

zx::result<> Nvme::Start() {
  parent_node_.Bind(std::move(node()));

  zx::result<fit::function<void()>> release = InitResources();
  if (release.is_error()) {
    return release.take_error();
  }
  auto cleanup = fit::defer([&] { (*release)(); });

  auto [controller_client_end, controller_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto [node_client_end, node_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::Node>::Create();

  node_controller_.Bind(std::move(controller_client_end));
  root_node_.Bind(std::move(node_client_end));

  fidl::Arena arena;

  const auto args =
      fuchsia_driver_framework::wire::NodeAddArgs::Builder(arena).name(arena, name()).Build();

  auto result =
      parent_node_->AddChild(args, std::move(controller_server_end), std::move(node_server_end));
  if (!result.ok()) {
    FDF_LOG(ERROR, "Failed to add child: %s", result.status_string());
    return zx::error(result.status());
  }

  zx_status_t status = Init();
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Driver initialization failed: %s", zx_status_get_string(status));
    return zx::error(status);
  }

  cleanup.cancel();
  return zx::ok();
}

}  // namespace nvme
