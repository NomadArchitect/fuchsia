// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ft_device.h"

#include <lib/ddk/binding_driver.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/hw/arch_ops.h>
#include <lib/ddk/metadata.h>
#include <lib/ddk/platform-defs.h>
#include <lib/ddk/trace/event.h>
#include <lib/fit/defer.h>
#include <lib/zx/clock.h>
#include <lib/zx/process.h>
#include <lib/zx/profile.h>
#include <lib/zx/time.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <zircon/compiler.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/threads.h>

#include <algorithm>
#include <iterator>

#include <fbl/algorithm.h>
#include <fbl/auto_lock.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>

namespace ft {

namespace {

constexpr fuchsia_input_report::wire::Axis XYAxis(int64_t max) {
  return {
      .range = {.min = 0, .max = max},
      .unit =
          {
              .type = fuchsia_input_report::wire::UnitType::kOther,
              .exponent = 0,
          },
  };
}

constexpr size_t kFt3x27XMax = 600;
constexpr size_t kFt3x27YMax = 1024;

constexpr size_t kFt6336XMax = 480;
constexpr size_t kFt6336YMax = 800;

constexpr size_t kFt5726XMax = 800;
constexpr size_t kFt5726YMax = 1280;

constexpr size_t kFt5336XMax = 1080;
constexpr size_t kFt5336YMax = 1920;

constexpr uint8_t kFtTouchEventTypeMask = 0xC0;
constexpr uint8_t kFtTouchEventTypeShift = 6;
enum FtTouchEventType : uint8_t {
  DOWN = 0,
  UP = 1,
  CONTACT = 2,
};

}  // namespace

void FtDevice::FtInputReport::ToFidlInputReport(
    fidl::WireTableBuilder<::fuchsia_input_report::wire::InputReport>& input_report,
    fidl::AnyArena& allocator) const {
  fidl::VectorView<fuchsia_input_report::wire::ContactInputReport> contact_rpt(allocator,
                                                                               contact_count);
  for (size_t i = 0; i < contact_count; i++) {
    contact_rpt[i] = fuchsia_input_report::wire::ContactInputReport::Builder(allocator)
                         .contact_id(contacts[i].finger_id)
                         .position_x(contacts[i].x)
                         .position_y(contacts[i].y)
                         .Build();
  }

  auto touch_report =
      fuchsia_input_report::wire::TouchInputReport::Builder(allocator).contacts(contact_rpt);
  input_report.event_time(event_time.get()).touch(touch_report.Build());
}

FtDevice::FtInputReport FtDevice::ParseReport(const uint8_t* buf) {
  FtInputReport report;
  const uint8_t contact_count = std::min(buf[0], static_cast<uint8_t>(report.contacts.max_size()));
  buf += 1;
  report.contact_count = 0;
  for (size_t i = 0; i < contact_count; i++, buf += kFingerRptSize) {
    if (((buf[0] & kFtTouchEventTypeMask) >> kFtTouchEventTypeShift) != FtTouchEventType::CONTACT) {
      continue;
    }
    report.contacts[i].x = static_cast<uint16_t>(((buf[0] & 0x0f) << 8) + buf[1]);
    report.contacts[i].y = static_cast<uint16_t>(((buf[2] & 0x0f) << 8) + buf[3]);
    report.contacts[i].finger_id = static_cast<uint8_t>(buf[2] >> 4);
    report.contact_count++;
  }
  return report;
}

void FtDevice::HandleIrq(async_dispatcher_t* dispatcher, async::IrqBase* irq, zx_status_t status,
                         const zx_packet_interrupt_t* interrupt) {
  if (status != ZX_OK) {
    zxlogf(ERROR, "focaltouch: Interrupt error %d", status);
  }
  TRACE_DURATION("input", "FtDevice Read");
  uint8_t i2c_buf[kMaxPoints * kFingerRptSize + 1];
  status = Read(FTS_REG_CURPOINT, i2c_buf, kMaxPoints * kFingerRptSize + 1);
  if (status == ZX_OK) {
    auto timestamp = zx::time(interrupt->timestamp);
    auto report = ParseReport(i2c_buf);
    report.event_time = timestamp;
    readers_.SendReportToAllReaders(report);

    const zx::duration latency = zx::clock::get_monotonic() - timestamp;

    total_latency_ += latency;
    report_count_++;
    average_latency_usecs_.Set(total_latency_.to_usecs() / report_count_);

    if (latency > max_latency_) {
      max_latency_ = latency;
      max_latency_usecs_.Set(max_latency_.to_usecs());
    }

    if (i2c_buf[0] > 0) {
      total_report_count_.Add(1);
      last_event_timestamp_.Set(timestamp.get());
    }
  } else {
    zxlogf(ERROR, "focaltouch: i2c read error");
  }

  irq_.ack();
}

zx_status_t FtDevice::Init() {
  i2c_ = ddk::I2cChannel(parent(), "i2c");
  if (!i2c_.is_valid()) {
    zxlogf(ERROR, "failed to acquire i2c");
    return ZX_ERR_NO_RESOURCES;
  }

  const char* kIntGpioFragmentName = "gpio-int";
  zx::result int_gpio_client =
      DdkConnectFragmentFidlProtocol<fuchsia_hardware_gpio::Service::Device>(parent(),
                                                                             kIntGpioFragmentName);
  if (int_gpio_client.is_error()) {
    zxlogf(ERROR, "Failed to get gpio protocol from fragment %s: %s", kIntGpioFragmentName,
           int_gpio_client.status_string());
    return ZX_ERR_NO_RESOURCES;
  }
  int_gpio_.Bind(std::move(int_gpio_client.value()));

  const char* kResetGpioFragmentName = "gpio-reset";
  auto reset_gpio_client = DdkConnectFragmentFidlProtocol<fuchsia_hardware_gpio::Service::Device>(
      parent(), kResetGpioFragmentName);
  if (reset_gpio_client.is_error()) {
    zxlogf(ERROR, "Failed to get gpio protocol from fragment %s: %s", kResetGpioFragmentName,
           reset_gpio_client.status_string());
    return ZX_ERR_NO_RESOURCES;
  }
  reset_gpio_.Bind(std::move(reset_gpio_client.value()));

  {
    fidl::WireResult result = int_gpio_->SetBufferMode(fuchsia_hardware_gpio::BufferMode::kInput);
    if (!result.ok()) {
      zxlogf(ERROR, "Failed to send SetBufferMode request to int gpio: %s", result.status_string());
      return result.status();
    }
    if (result->is_error()) {
      zxlogf(ERROR, "Failed to configure int gpio to input: %s",
             zx_status_get_string(result->error_value()));
      return result->error_value();
    }
  }

  {
    fidl::Arena arena;
    auto config = fuchsia_hardware_gpio::wire::InterruptConfiguration::Builder(arena)
                      .mode(fuchsia_hardware_gpio::InterruptMode::kEdgeLow)
                      .Build();
    fidl::WireResult result = int_gpio_->ConfigureInterrupt(config);
    if (!result.ok()) {
      zxlogf(ERROR, "Failed to send ConfigureInterrupt request to int gpio: %s",
             result.status_string());
      return result.status();
    }
    if (result->is_error()) {
      zxlogf(ERROR, "Failed to configure int gpio: %s",
             zx_status_get_string(result->error_value()));
      return result->error_value();
    }
  }

  fidl::WireResult interrupt = int_gpio_->GetInterrupt({});
  if (!interrupt.ok()) {
    zxlogf(ERROR, "Failed to send GetInterrupt request to int gpio: %s", interrupt.status_string());
    return interrupt.status();
  }
  if (interrupt->is_error()) {
    zxlogf(ERROR, "Failed to get interrupt from int gpio: %s",
           zx_status_get_string(interrupt->error_value()));
    return interrupt->error_value();
  }
  irq_ = std::move(interrupt.value()->interrupt);
  irq_handler_.set_object(irq_.get());
  irq_handler_.Begin(dispatcher_);

  size_t actual;
  FocaltechMetadata device_info;
  zx_status_t status = device_get_metadata(parent(), DEVICE_METADATA_PRIVATE, &device_info,
                                           sizeof(device_info), &actual);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Failed to get metadata: %s", zx_status_get_string(status));
    return status;
  }
  if (sizeof(device_info) != actual) {
    zxlogf(ERROR, "Incorrect metadata size: Expected %lu bytes but actual is %lu bytes",
           sizeof(device_info), actual);
    return ZX_ERR_INTERNAL;
  }

  if (device_info.device_id == FOCALTECH_DEVICE_FT3X27) {
    x_max_ = kFt3x27XMax;
    y_max_ = kFt3x27YMax;
  } else if (device_info.device_id == FOCALTECH_DEVICE_FT6336) {
    x_max_ = kFt6336XMax;
    y_max_ = kFt6336YMax;
  } else if (device_info.device_id == FOCALTECH_DEVICE_FT5726) {
    x_max_ = kFt5726XMax;
    y_max_ = kFt5726YMax;
  } else if (device_info.device_id == FOCALTECH_DEVICE_FT5336) {
    // Currently we assume the panel to be always Khadas TS050. If this changes,
    // we may need extra information from the metadata to determine which HID
    // report descriptor to use.
    x_max_ = kFt5336XMax;
    y_max_ = kFt5336YMax;
  } else {
    zxlogf(ERROR, "focaltouch: unknown device ID %u", device_info.device_id);
    return ZX_ERR_INTERNAL;
  }

  // Reset the chip -- should be low for at least 1ms, and the chip should take at most 200ms to
  // initialize.
  {
    fidl::WireResult result =
        reset_gpio_->SetBufferMode(fuchsia_hardware_gpio::BufferMode::kOutputLow);
    if (!result.ok()) {
      zxlogf(ERROR, "Failed to send SetBufferMode request to reset gpio: %s",
             result.status_string());
      return result.status();
    }
    if (result->is_error()) {
      zxlogf(ERROR, "Failed to configure reset gpio to output: %s",
             zx_status_get_string(result->error_value()));
      return result->error_value();
    }
  }
  zx::nanosleep(zx::deadline_after(zx::msec(5)));
  {
    fidl::WireResult result =
        reset_gpio_->SetBufferMode(fuchsia_hardware_gpio::BufferMode::kOutputHigh);
    if (!result.ok()) {
      zxlogf(ERROR, "Failed to send Write request to reset gpio: %s", result.status_string());
      return result.status();
    }
    if (result->is_error()) {
      zxlogf(ERROR, "Failed to write to reset gpio: %s",
             zx_status_get_string(result->error_value()));
      return result->error_value();
    }
  }
  zx::nanosleep(zx::deadline_after(zx::msec(200)));

  status = UpdateFirmwareIfNeeded(device_info);
  if (status != ZX_OK) {
    return status;
  }

  node_ = inspector_.GetRoot().CreateChild("Chip_info");
  LogRegisterValue(FTS_REG_TYPE, "TYPE");
  LogRegisterValue(FTS_REG_FIRMID, "FIRMID");
  LogRegisterValue(FTS_REG_VENDOR_ID, "VENDOR_ID");
  LogRegisterValue(FTS_REG_PANEL_ID, "PANEL_ID");
  LogRegisterValue(FTS_REG_RELEASE_ID_HIGH, "RELEASE_ID_HIGH");
  LogRegisterValue(FTS_REG_RELEASE_ID_LOW, "RELEASE_ID_LOW");
  LogRegisterValue(FTS_REG_IC_VERSION, "IC_VERSION");

  if (device_info.needs_firmware) {
    node_.CreateUint("Display_vendor", device_info.display_vendor, &values_);
    node_.CreateUint("DDIC_version", device_info.ddic_version, &values_);
    zxlogf(INFO, "Display vendor: %u", device_info.display_vendor);
    zxlogf(INFO, "DDIC version:   %u", device_info.ddic_version);
  } else {
    node_.CreateString("Display_vendor", "none", &values_);
    node_.CreateString("DDIC_version", "none", &values_);
    zxlogf(INFO, "Display vendor: none");
    zxlogf(INFO, "DDIC version:   none");
  }

  // These names must match the strings in //src/diagnostics/config/sampler/input.json.
  metrics_root_ = inspector_.GetRoot().CreateChild("hid-input-report-touch");
  average_latency_usecs_ = metrics_root_.CreateUint("average_latency_usecs", 0);
  max_latency_usecs_ = metrics_root_.CreateUint("max_latency_usecs", 0);
  total_report_count_ = metrics_root_.CreateUint("total_report_count", 0);
  last_event_timestamp_ = metrics_root_.CreateUint("last_event_timestamp", 0);

  return ZX_OK;
}

zx_status_t FtDevice::Create(void* ctx, zx_device_t* device) {
  zxlogf(INFO, "focaltouch: driver started...");

  auto ft_dev = std::make_unique<FtDevice>(
      device, fdf_dispatcher_get_async_dispatcher(fdf_dispatcher_get_current_dispatcher()));
  zx_status_t status = ft_dev->Init();
  if (status != ZX_OK) {
    zxlogf(ERROR, "focaltouch: Driver bind failed %d", status);
    return status;
  }

  auto cleanup = fit::defer([&]() { ft_dev->ShutDown(); });

  status = ft_dev->DdkAdd(ddk::DeviceAddArgs("focaltouch-HidDevice")
                              .set_inspect_vmo(ft_dev->inspector_.DuplicateVmo()));
  if (status != ZX_OK) {
    zxlogf(ERROR, "focaltouch: Could not create hid device: %d", status);
    return status;
  } else {
    zxlogf(INFO, "focaltouch: Added hid device");
  }

  cleanup.cancel();

  // device intentionally leaked as it is now held by DevMgr
  [[maybe_unused]] auto ptr = ft_dev.release();

  return ZX_OK;
}

void FtDevice::DdkRelease() { delete this; }

void FtDevice::DdkUnbind(ddk::UnbindTxn txn) {
  ShutDown();
  txn.Reply();
}

zx_status_t FtDevice::ShutDown() {
  irq_handler_.Cancel();
  irq_.destroy();
  return ZX_OK;
}

void FtDevice::GetInputReportsReader(GetInputReportsReaderRequestView request,
                                     GetInputReportsReaderCompleter::Sync& completer) {
  auto status = readers_.CreateReader(dispatcher_, std::move(request->reader));
  if (status == ZX_OK) {
#ifdef FT_TEST
    sync_completion_signal(&next_reader_wait_);
#endif
  }
}

void FtDevice::GetDescriptor(GetDescriptorCompleter::Sync& completer) {
  fidl::Arena<kFeatureAndDescriptorBufferSize> allocator;

  auto device_info = fuchsia_input_report::wire::DeviceInformation::Builder(allocator);
  device_info.vendor_id(static_cast<uint32_t>(fuchsia_input_report::wire::VendorId::kGoogle));
  device_info.product_id(static_cast<uint32_t>(
      fuchsia_input_report::wire::VendorGoogleProductId::kFocaltechTouchscreen));

  fidl::VectorView<fuchsia_input_report::wire::ContactInputDescriptor> contacts(allocator,
                                                                                kMaxPoints);
  for (auto& c : contacts) {
    c = fuchsia_input_report::wire::ContactInputDescriptor::Builder(allocator)
            .position_x(XYAxis(x_max_))
            .position_y(XYAxis(y_max_))
            .Build();
  }

  const auto input = fuchsia_input_report::wire::TouchInputDescriptor::Builder(allocator)
                         .touch_type(fuchsia_input_report::wire::TouchType::kTouchscreen)
                         .max_contacts(kMaxPoints)
                         .contacts(contacts)
                         .Build();

  const auto touch =
      fuchsia_input_report::wire::TouchDescriptor::Builder(allocator).input(input).Build();

  const auto descriptor = fuchsia_input_report::wire::DeviceDescriptor::Builder(allocator)
                              .device_information(device_info.Build())
                              .touch(touch)
                              .Build();

  completer.Reply(descriptor);
}

// simple i2c read for reading one register location
//  intended mostly for debug purposes
uint8_t FtDevice::Read(uint8_t addr) {
  uint8_t rbuf;
  i2c_.WriteReadSync(&addr, 1, &rbuf, 1);
  return rbuf;
}

zx_status_t FtDevice::Read(uint8_t addr, uint8_t* buf, size_t len) {
  // TODO(bradenkell): Remove this workaround when transfers of more than 8 bytes are supported on
  // the MT8167.
  while (len > 0) {
    size_t readlen = std::min(len, kMaxI2cTransferLength);

    zx_status_t status = i2c_.WriteReadSync(&addr, 1, buf, readlen);
    if (status != ZX_OK) {
      zxlogf(ERROR, "Failed to read i2c - %d", status);
      return status;
    }

    addr = static_cast<uint8_t>(addr + readlen);
    buf += readlen;
    len -= readlen;
  }

  return ZX_OK;
}

void FtDevice::LogRegisterValue(uint8_t addr, const char* name) {
  uint8_t value;
  zx_status_t status = Read(addr, &value, sizeof(value));
  if (status == ZX_OK) {
    node_.CreateByteVector(name, {&value, sizeof(value)}, &values_);
    zxlogf(INFO, "  %-16s: 0x%02x", name, value);
  } else {
    node_.CreateString(name, "error", &values_);
    zxlogf(ERROR, "  %-16s: error %d", name, status);
  }
}

static constexpr zx_driver_ops_t driver_ops = []() {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = FtDevice::Create;
  return ops;
}();

}  // namespace ft

ZIRCON_DRIVER(focaltech_touch, ft::driver_ops, "focaltech-touch", "0.1");
