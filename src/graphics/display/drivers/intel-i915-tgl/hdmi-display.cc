// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/intel-i915-tgl/hdmi-display.h"

#include <lib/ddk/debug.h>
#include <lib/ddk/driver.h>
#include <lib/mmio/mmio-buffer.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/time.h>
#include <zircon/assert.h>
#include <zircon/errors.h>

#include <cmath>
#include <cstdint>
#include <iterator>
#include <limits>
#include <optional>

#include <fbl/auto_lock.h>

#include "src/graphics/display/drivers/intel-i915-tgl/ddi-physical-layer-manager.h"
#include "src/graphics/display/drivers/intel-i915-tgl/dpll-config.h"
#include "src/graphics/display/drivers/intel-i915-tgl/dpll.h"
#include "src/graphics/display/drivers/intel-i915-tgl/gmbus-gpio.h"
#include "src/graphics/display/drivers/intel-i915-tgl/hardware-common.h"
#include "src/graphics/display/drivers/intel-i915-tgl/intel-i915-tgl.h"
#include "src/graphics/display/drivers/intel-i915-tgl/pci-ids.h"
#include "src/graphics/display/drivers/intel-i915-tgl/poll-until.h"
#include "src/graphics/display/drivers/intel-i915-tgl/registers-ddi.h"
#include "src/graphics/display/drivers/intel-i915-tgl/registers-dpll.h"
#include "src/graphics/display/drivers/intel-i915-tgl/registers-gmbus.h"
#include "src/graphics/display/drivers/intel-i915-tgl/registers-pipe.h"
#include "src/graphics/display/drivers/intel-i915-tgl/registers-transcoder.h"
#include "src/graphics/display/drivers/intel-i915-tgl/registers.h"

namespace i915_tgl {

// I2c functions

namespace {

// Recommended DDI buffer translation programming values

struct DdiPhyConfigEntry {
  uint32_t entry2;
  uint32_t entry1;
};

// The tables below have the values recommended by the documentation.
//
// Kaby Lake: IHD-OS-KBL-Vol 12-1.17 pages 187-190
// Skylake: IHD-OS-SKL-Vol 12-05.16 pages 181-183
//
// TODO(fxbug.dev/108252): Per-entry Iboost values.

constexpr DdiPhyConfigEntry kPhyConfigHdmiSkylakeUhs[11] = {
    {0x000000ac, 0x00000018}, {0x0000009d, 0x00005012}, {0x00000088, 0x00007011},
    {0x000000a1, 0x00000018}, {0x00000098, 0x00000018}, {0x00000088, 0x00004013},
    {0x000000cd, 0x80006012}, {0x000000df, 0x00000018}, {0x000000cd, 0x80003015},
    {0x000000c0, 0x80003015}, {0x000000c0, 0x80000018},
};

constexpr DdiPhyConfigEntry kPhyConfigHdmiSkylakeY[11] = {
    {0x000000a1, 0x00000018}, {0x000000df, 0x00005012}, {0x000000cb, 0x80007011},
    {0x000000a4, 0x00000018}, {0x0000009d, 0x00000018}, {0x00000080, 0x00004013},
    {0x000000c0, 0x80006012}, {0x0000008a, 0x00000018}, {0x000000c0, 0x80003015},
    {0x000000c0, 0x80003015}, {0x000000c0, 0x80000018},
};

cpp20::span<const DdiPhyConfigEntry> GetHdmiPhyConfigEntries(uint16_t device_id,
                                                             uint8_t* default_iboost) {
  if (is_skl_y(device_id) || is_kbl_y(device_id)) {
    *default_iboost = 3;
    return kPhyConfigHdmiSkylakeY;
  }

  *default_iboost = 1;
  return kPhyConfigHdmiSkylakeUhs;
}

void WriteGMBusData(fdf::MmioBuffer* mmio_space, const uint8_t* buf, uint32_t size, uint32_t idx) {
  if (idx >= size) {
    return;
  }
  cpp20::span<const uint8_t> data(buf + idx, std::min(4u, size - idx));
  tgl_registers::GMBusData::Get().FromValue(0).set_data(data).WriteTo(mmio_space);
}

void ReadGMBusData(fdf::MmioBuffer* mmio_space, uint8_t* buf, uint32_t size, uint32_t idx) {
  int cur_byte = 0;
  auto bytes = tgl_registers::GMBusData::Get().ReadFrom(mmio_space).data();
  while (idx < size && cur_byte < 4) {
    buf[idx++] = bytes[cur_byte++];
  }
}

static constexpr uint8_t kDdcSegmentAddress = 0x30;
static constexpr uint8_t kDdcDataAddress = 0x50;
static constexpr uint8_t kI2cClockUs = 10;  // 100 kHz

// For bit banging i2c over the gpio pins
bool i2c_scl(fdf::MmioBuffer* mmio_space, const GpioPort& gpio_port, bool hi) {
  auto gpio_pin_pair_control =
      tgl_registers::GpioPinPairControl::GetForPort(gpio_port).FromValue(0);

  if (!hi) {
    gpio_pin_pair_control.set_clock_direction_is_output(true);
    gpio_pin_pair_control.set_write_clock_output(true);
  }
  gpio_pin_pair_control.set_write_clock_direction_is_output(true);

  gpio_pin_pair_control.WriteTo(mmio_space);
  gpio_pin_pair_control.ReadFrom(mmio_space);  // Posting read

  // Handle the case where something on the bus is holding the clock
  // low. Timeout after 1ms.
  if (hi) {
    int count = 0;
    do {
      if (count != 0) {
        zx_nanosleep(zx_deadline_after(ZX_USEC(kI2cClockUs)));
      }
      gpio_pin_pair_control.ReadFrom(mmio_space);
    } while (count++ < 100 && hi != gpio_pin_pair_control.clock_input());
    if (hi != gpio_pin_pair_control.clock_input()) {
      return false;
    }
  }
  zx_nanosleep(zx_deadline_after(ZX_USEC(kI2cClockUs / 2)));
  return true;
}

// For bit banging i2c over the gpio pins
void i2c_sda(fdf::MmioBuffer* mmio_space, const GpioPort& gpio_port, bool hi) {
  auto gpio_pin_pair_control =
      tgl_registers::GpioPinPairControl::GetForPort(gpio_port).FromValue(0);

  if (!hi) {
    gpio_pin_pair_control.set_data_direction_is_output(true);
    gpio_pin_pair_control.set_write_data_output(true);
  }
  gpio_pin_pair_control.set_write_data_direction_is_output(true);

  gpio_pin_pair_control.WriteTo(mmio_space);
  gpio_pin_pair_control.ReadFrom(mmio_space);  // Posting read

  zx_nanosleep(zx_deadline_after(ZX_USEC(kI2cClockUs / 2)));
}

// For bit banging i2c over the gpio pins
bool i2c_send_byte(fdf::MmioBuffer* mmio_space, const GpioPort& gpio_port, uint8_t byte) {
  // Set the bits from MSB to LSB
  for (int i = 7; i >= 0; i--) {
    i2c_sda(mmio_space, gpio_port, (byte >> i) & 0x1);

    i2c_scl(mmio_space, gpio_port, 1);

    // Leave the data line where it is for the rest of the cycle
    zx_nanosleep(zx_deadline_after(ZX_USEC(kI2cClockUs / 2)));

    i2c_scl(mmio_space, gpio_port, 0);
  }

  // Release the data line and check for an ack
  i2c_sda(mmio_space, gpio_port, 1);
  i2c_scl(mmio_space, gpio_port, 1);

  bool ack =
      !tgl_registers::GpioPinPairControl::GetForPort(gpio_port).ReadFrom(mmio_space).data_input();

  // Sleep for the rest of the cycle
  zx_nanosleep(zx_deadline_after(ZX_USEC(kI2cClockUs / 2)));

  i2c_scl(mmio_space, gpio_port, 0);

  return ack;
}

}  // namespace

// Per the GMBUS Controller Programming Interface section of the Intel docs, GMBUS does not
// directly support segment pointer addressing. Instead, the segment pointer needs to be
// set by bit-banging the GPIO pins.
bool GMBusI2c::SetDdcSegment(uint8_t segment_num) {
  ZX_ASSERT(gpio_port_.has_value());

  // Reset the clock and data lines
  i2c_scl(mmio_space_, *gpio_port_, 0);
  i2c_sda(mmio_space_, *gpio_port_, 0);

  if (!i2c_scl(mmio_space_, *gpio_port_, 1)) {
    return false;
  }
  i2c_sda(mmio_space_, *gpio_port_, 1);
  // Wait for the rest of the cycle
  zx_nanosleep(zx_deadline_after(ZX_USEC(kI2cClockUs / 2)));

  // Send a start condition
  i2c_sda(mmio_space_, *gpio_port_, 0);
  i2c_scl(mmio_space_, *gpio_port_, 0);

  // Send the segment register index and the segment number
  uint8_t segment_write_command = kDdcSegmentAddress << 1;
  if (!i2c_send_byte(mmio_space_, *gpio_port_, segment_write_command) ||
      !i2c_send_byte(mmio_space_, *gpio_port_, segment_num)) {
    return false;
  }

  // Set the data and clock lines high to prepare for the GMBus start
  i2c_sda(mmio_space_, *gpio_port_, 1);
  return i2c_scl(mmio_space_, *gpio_port_, 1);
}

zx_status_t GMBusI2c::I2cTransact(const i2c_impl_op_t* ops, size_t size) {
  ZX_ASSERT(gmbus_pin_pair_.has_value());
  ZX_ASSERT(gpio_port_.has_value());

  // The GMBus register is a limited interface to the i2c bus - it doesn't support complex
  // transactions like setting the E-DDC segment. For now, providing a special-case interface
  // for reading the E-DDC is good enough.
  fbl::AutoLock lock(&lock_);
  zx_status_t fail_res = ZX_ERR_IO;
  bool gmbus_set = false;
  for (unsigned i = 0; i < size; i++) {
    const i2c_impl_op_t* op = ops + i;
    if (op->address == kDdcSegmentAddress && !op->is_read && op->data_size == 1) {
      tgl_registers::GMBusClockPortSelect::Get().FromValue(0).WriteTo(mmio_space_);
      gmbus_set = false;
      if (!SetDdcSegment(*static_cast<uint8_t*>(op->data_buffer))) {
        goto fail;
      }
    } else if (op->address == kDdcDataAddress) {
      if (!gmbus_set) {
        auto gmbus_clock_port_select = tgl_registers::GMBusClockPortSelect::Get().FromValue(0);
        gmbus_clock_port_select.SetPinPair(*gmbus_pin_pair_).WriteTo(mmio_space_);

        gmbus_set = true;
      }

      uint8_t* buf = static_cast<uint8_t*>(op->data_buffer);
      uint8_t len = static_cast<uint8_t>(op->data_size);
      if (op->is_read ? GMBusRead(kDdcDataAddress, buf, len)
                      : GMBusWrite(kDdcDataAddress, buf, len)) {
        // Alias `mmio_space_` to aid Clang's thread safety analyzer, which
        // can't reason about closure scopes. The type system helps ensure
        // thread-safety, because the scope of the alias is included in the
        // scope of the AutoLock.
        fdf::MmioBuffer& mmio_space = *mmio_space_;

        if (!PollUntil(
                [&]() {
                  return tgl_registers::GMBusControllerStatus::Get()
                      .ReadFrom(&mmio_space)
                      .is_waiting();
                },
                zx::msec(1), 10)) {
          zxlogf(TRACE, "Transition to wait phase timed out");
          goto fail;
        }
      } else {
        goto fail;
      }
    } else {
      fail_res = ZX_ERR_NOT_SUPPORTED;
      goto fail;
    }

    if (op->stop) {
      if (!I2cFinish()) {
        goto fail;
      }
      gmbus_set = false;
    }
  }

  return ZX_OK;
fail:
  if (!I2cClearNack()) {
    zxlogf(TRACE, "Failed to clear nack");
  }
  return fail_res;
}

bool GMBusI2c::GMBusWrite(uint8_t addr, const uint8_t* buf, uint8_t size) {
  unsigned idx = 0;
  WriteGMBusData(mmio_space_, buf, size, idx);
  idx += 4;

  auto gmbus_command = tgl_registers::GMBusCommand::Get().FromValue(0);
  gmbus_command.set_software_ready(true);
  gmbus_command.set_wait_state_enabled(true);
  gmbus_command.set_total_byte_count(size);
  gmbus_command.set_target_address(addr);
  gmbus_command.WriteTo(mmio_space_);

  while (idx < size) {
    if (!I2cWaitForHwReady()) {
      return false;
    }

    WriteGMBusData(mmio_space_, buf, size, idx);
    idx += 4;
  }
  // One more wait to ensure we're ready when we leave the function
  return I2cWaitForHwReady();
}

bool GMBusI2c::GMBusRead(uint8_t addr, uint8_t* buf, uint8_t size) {
  auto gmbus_command = tgl_registers::GMBusCommand::Get().FromValue(0);
  gmbus_command.set_software_ready(true);
  gmbus_command.set_wait_state_enabled(true);
  gmbus_command.set_total_byte_count(size);
  gmbus_command.set_target_address(addr);
  gmbus_command.set_is_read_transaction(true);
  gmbus_command.WriteTo(mmio_space_);

  unsigned idx = 0;
  while (idx < size) {
    if (!I2cWaitForHwReady()) {
      return false;
    }

    ReadGMBusData(mmio_space_, buf, size, idx);
    idx += 4;
  }

  return true;
}

bool GMBusI2c::I2cFinish() {
  auto gmbus_command = tgl_registers::GMBusCommand::Get().FromValue(0);
  gmbus_command.set_stop_generated(true);
  gmbus_command.set_software_ready(true);
  gmbus_command.WriteTo(mmio_space_);

  // Alias `mmio_space_` to aid Clang's thread safety analyzer, which can't
  // reason about closure scopes. The type system still helps ensure
  // thread-safety, because the scope of the alias is smaller than the method
  // scope, and the method is guaranteed to hold the lock.
  fdf::MmioBuffer& mmio_space = *mmio_space_;
  bool idle = PollUntil(
      [&] {
        return !tgl_registers::GMBusControllerStatus::Get().ReadFrom(&mmio_space).is_active();
      },
      zx::msec(1), 100);

  auto gmbus_clock_port_select = tgl_registers::GMBusClockPortSelect::Get().FromValue(0);
  gmbus_clock_port_select.set_pin_pair_select(0);
  gmbus_clock_port_select.WriteTo(mmio_space_);

  if (!idle) {
    zxlogf(TRACE, "hdmi: GMBus i2c failed to go idle");
  }
  return idle;
}

bool GMBusI2c::I2cWaitForHwReady() {
  auto gmbus_controller_status = tgl_registers::GMBusControllerStatus::Get().FromValue(0);

  // Alias `mmio_space_` to aid Clang's thread safety analyzer, which can't
  // reason about closure scopes. The type system still helps ensure
  // thread-safety, because the scope of the alias is smaller than the method
  // scope, and the method is guaranteed to hold the lock.
  fdf::MmioBuffer& mmio_space = *mmio_space_;

  if (!PollUntil(
          [&] {
            gmbus_controller_status.ReadFrom(&mmio_space);
            return gmbus_controller_status.nack_occurred() || gmbus_controller_status.is_ready();
          },
          zx::msec(1), 50)) {
    zxlogf(TRACE, "hdmi: GMBus i2c wait for hwready timeout");
    return false;
  }
  if (gmbus_controller_status.nack_occurred()) {
    zxlogf(TRACE, "hdmi: GMBus i2c got nack");
    return false;
  }
  return true;
}

bool GMBusI2c::I2cClearNack() {
  I2cFinish();

  // Alias `mmio_space_` to aid Clang's thread safety analyzer, which can't
  // reason about closure scopes. The type system still helps ensure
  // thread-safety, because the scope of the alias is smaller than the method
  // scope, and the method is guaranteed to hold the lock.
  fdf::MmioBuffer& mmio_space = *mmio_space_;

  if (!PollUntil(
          [&] {
            return !tgl_registers::GMBusControllerStatus::Get().ReadFrom(&mmio_space).is_active();
          },
          zx::msec(1), 10)) {
    zxlogf(TRACE, "hdmi: GMBus i2c failed to clear active nack");
    return false;
  }

  // Set/clear sw clear int to reset the bus
  auto gmbus_command = tgl_registers::GMBusCommand::Get().FromValue(0);
  gmbus_command.set_software_clear_interrupt(true);
  gmbus_command.WriteTo(mmio_space_);
  gmbus_command.set_software_clear_interrupt(false);
  gmbus_command.WriteTo(mmio_space_);

  // Reset GMBus0
  auto gmbus_clock_port_select = tgl_registers::GMBusClockPortSelect::Get().FromValue(0);
  gmbus_clock_port_select.WriteTo(mmio_space_);

  return true;
}

GMBusI2c::GMBusI2c(DdiId ddi_id, tgl_registers::Platform platform, fdf::MmioBuffer* mmio_space)
    : gmbus_pin_pair_(GMBusPinPair::GetForDdi(ddi_id, platform)),
      gpio_port_(GpioPort::GetForDdi(ddi_id, platform)),
      mmio_space_(mmio_space) {
  ZX_ASSERT(mtx_init(&lock_, mtx_plain) == thrd_success);
}

// Modesetting functions

// On DisplayDevice creation we cannot determine whether it is an HDMI
// display; this will be updated when intel-i915 Controller gets EDID
// information for this device (before Init()).
HdmiDisplay::HdmiDisplay(Controller* controller, uint64_t id, DdiId ddi_id,
                         DdiReference ddi_reference)
    : DisplayDevice(controller, id, ddi_id, std::move(ddi_reference), Type::kHdmi) {}

HdmiDisplay::~HdmiDisplay() = default;

bool HdmiDisplay::Query() {
  // HDMI isn't supported on these DDIs
  const tgl_registers::Platform platform = GetPlatform(controller()->device_id());
  if (!GMBusPinPair::HasValidPinPair(ddi_id(), platform)) {
    return false;
  }

  // Reset the GMBus registers and disable GMBus interrupts
  tgl_registers::GMBusClockPortSelect::Get().FromValue(0).WriteTo(mmio_space());
  tgl_registers::GMBusControllerInterruptMask::Get().FromValue(0).WriteTo(mmio_space());

  // The only way to tell if an HDMI monitor is actually connected is
  // to try to read from it over I2C.
  for (unsigned i = 0; i < 3; i++) {
    uint8_t test_data = 0;
    i2c_impl_op_t op = {
        .address = kDdcDataAddress,
        .data_buffer = &test_data,
        .data_size = 1,
        .is_read = true,
        .stop = 1,
    };
    tgl_registers::GMBusClockPortSelect::Get().FromValue(0).WriteTo(mmio_space());
    // TODO(fxbug.dev/99979): We should read using GMBusI2c directly instead.
    if (controller()->Transact(i2c_bus_id(), &op, 1) == ZX_OK) {
      zxlogf(TRACE, "Found a hdmi/dvi monitor");
      return true;
    }
    zx_nanosleep(zx_deadline_after(ZX_MSEC(5)));
  }
  zxlogf(TRACE, "Failed to query hdmi i2c bus");
  return false;
}

bool HdmiDisplay::InitDdi() {
  // All the init happens during modeset
  return true;
}

bool HdmiDisplay::DdiModeset(const display_mode_t& mode) {
  pipe()->Reset();
  controller()->ResetDdi(ddi_id(), pipe()->connected_transcoder_id());

  const int32_t pixel_clock_khz = mode.pixel_clock_10khz * 10;
  DdiPllConfig pll_config = {
      .ddi_clock_khz = pixel_clock_khz * 5,
      .spread_spectrum_clocking = false,
      .admits_display_port = false,
      .admits_hdmi = true,
  };

  DisplayPll* dpll =
      controller()->dpll_manager()->SetDdiPllConfig(ddi_id(), /*is_edp=*/false, pll_config);
  if (dpll == nullptr) {
    return false;
  }

  ZX_DEBUG_ASSERT(controller()->power());
  controller()->power()->SetDdiIoPowerState(ddi_id(), /*enable=*/true);
  if (!PollUntil([&] { return controller()->power()->GetDdiIoPowerState(ddi_id()); }, zx::usec(1),
                 20)) {
    zxlogf(ERROR, "DDI %d IO power did not come up in 20us", ddi_id());
    return false;
  }

  controller()->power()->SetAuxIoPowerState(ddi_id(), /*enable=*/true);
  if (!PollUntil([&] { return controller()->power()->GetAuxIoPowerState(ddi_id()); }, zx::usec(1),
                 10)) {
    zxlogf(ERROR, "DDI %d IO power did not come up in 10us", ddi_id());
    return false;
  }

  return true;
}

bool HdmiDisplay::PipeConfigPreamble(const display_mode_t& mode, PipeId pipe_id,
                                     TranscoderId transcoder_id) {
  ZX_DEBUG_ASSERT_MSG(transcoder_id != TranscoderId::TRANSCODER_EDP,
                      "The EDP transcoder doesn't do HDMI");

  tgl_registers::TranscoderRegs transcoder_regs(transcoder_id);

  // Configure Transcoder Clock Select
  auto transcoder_clock_select = transcoder_regs.ClockSelect().ReadFrom(mmio_space());
  if (is_tgl(controller()->device_id())) {
    transcoder_clock_select.set_ddi_clock_tiger_lake(ddi_id());
  } else {
    transcoder_clock_select.set_ddi_clock_kaby_lake(ddi_id());
  }
  transcoder_clock_select.WriteTo(mmio_space());

  return true;
}

bool HdmiDisplay::PipeConfigEpilogue(const display_mode_t& mode, PipeId pipe_id,
                                     TranscoderId transcoder_id) {
  ZX_DEBUG_ASSERT(type() == DisplayDevice::Type::kHdmi || type() == DisplayDevice::Type::kDvi);
  ZX_DEBUG_ASSERT_MSG(transcoder_id != TranscoderId::TRANSCODER_EDP,
                      "The EDP transcoder doesn't do HDMI");

  tgl_registers::TranscoderRegs transcoder_regs(transcoder_id);

  auto transcoder_ddi_control = transcoder_regs.DdiControl().ReadFrom(mmio_space());
  transcoder_ddi_control.set_enabled(true);
  if (is_tgl(controller()->device_id())) {
    transcoder_ddi_control.set_ddi_tiger_lake(ddi_id());
  } else {
    transcoder_ddi_control.set_ddi_kaby_lake(ddi_id());
  }
  transcoder_ddi_control.set_ddi_mode(type() == DisplayDevice::Type::kHdmi
                                          ? tgl_registers::TranscoderDdiControl::kModeHdmi
                                          : tgl_registers::TranscoderDdiControl::kModeDvi);
  transcoder_ddi_control.set_bits_per_color(tgl_registers::TranscoderDdiControl::k8bpc)
      .set_vsync_polarity_not_inverted((mode.flags & MODE_FLAG_VSYNC_POSITIVE) != 0)
      .set_hsync_polarity_not_inverted((mode.flags & MODE_FLAG_HSYNC_POSITIVE) != 0)
      .set_is_port_sync_secondary_kaby_lake(false)
      .set_allocate_display_port_virtual_circuit_payload(false)
      .WriteTo(mmio_space());

  auto transcoder_config = transcoder_regs.Config().ReadFrom(mmio_space());
  transcoder_config.set_enabled_target(true)
      .set_interlaced_display((mode.flags & MODE_FLAG_INTERLACED) != 0)
      .WriteTo(mmio_space());

  // Configure voltage swing and related IO settings.
  //
  // TODO(fxbug.dev/114460): Move voltage swing configuration logic to a
  // DDI-specific class.

  // kUseDefaultIdx always fails the idx-in-bounds check, so no additional handling is needed
  uint8_t idx = controller()->igd_opregion().GetHdmiBufferTranslationIndex(ddi_id());
  uint8_t i_boost_override = controller()->igd_opregion().GetIBoost(ddi_id(), false /* is_dp */);

  uint8_t default_iboost;
  const cpp20::span<const DdiPhyConfigEntry> entries =
      GetHdmiPhyConfigEntries(controller()->device_id(), &default_iboost);
  if (idx >= entries.size()) {
    idx = 8;  // Default index
  }

  tgl_registers::DdiRegs ddi_regs(ddi_id());
  auto phy_config_entry1 = ddi_regs.PhyConfigEntry1(9).FromValue(0);
  phy_config_entry1.set_reg_value(entries[idx].entry1);
  if (i_boost_override) {
    phy_config_entry1.set_balance_leg_enable(1);
  }
  phy_config_entry1.WriteTo(mmio_space());

  auto phy_config_entry2 = ddi_regs.PhyConfigEntry2(9).FromValue(0);
  phy_config_entry2.set_reg_value(entries[idx].entry2).WriteTo(mmio_space());

  auto phy_balance_control = tgl_registers::DdiPhyBalanceControl::Get().ReadFrom(mmio_space());
  phy_balance_control.set_disable_balance_leg(0);
  phy_balance_control.balance_leg_select_for_ddi(ddi_id()).set(i_boost_override ? i_boost_override
                                                                                : default_iboost);
  phy_balance_control.WriteTo(mmio_space());

  // Configure and enable DDI_BUF_CTL
  auto buffer_control = ddi_regs.BufferControl().ReadFrom(mmio_space());
  buffer_control.set_enabled(true);
  buffer_control.WriteTo(mmio_space());

  return true;
}

DdiPllConfig HdmiDisplay::ComputeDdiPllConfig(int32_t pixel_clock_10khz) {
  const int32_t pixel_clock_khz = pixel_clock_10khz * 10;
  return DdiPllConfig{
      .ddi_clock_khz = pixel_clock_khz * 5,
      .spread_spectrum_clocking = false,
      .admits_display_port = false,
      .admits_hdmi = true,
  };
}

bool HdmiDisplay::CheckPixelRate(uint64_t pixel_rate_hz) {
  // Pixel rates of 300M/165M pixels per second for HDMI/DVI. The Intel docs state
  // that the maximum link bit rate of an HDMI port is 3GHz, not 3.4GHz that would
  // be expected  based on the HDMI spec.
  if ((type() == DisplayDevice::Type::kHdmi ? 300'000'000 : 165'000'000) < pixel_rate_hz) {
    return false;
  }

  DdiPllConfig pll_config = ComputeDdiPllConfig(static_cast<int32_t>(pixel_rate_hz / 10'000));
  if (pll_config.IsEmpty()) {
    return false;
  }

  DpllOscillatorConfig dco_config = CreateDpllOscillatorConfigKabyLake(pll_config.ddi_clock_khz);
  return dco_config.frequency_khz != 0;
}

}  // namespace i915_tgl
