// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_UART_PL011_H_
#define LIB_UART_PL011_H_

#include <lib/stdcompat/array.h>
#include <lib/zbi-format/driver-config.h>
#include <lib/zbi-format/zbi.h>

#include <cstdint>
#include <string_view>

#include <hwreg/bitfields.h>

#include "interrupt.h"
#include "uart.h"

// PrimeCell® UART (PL011) Technical Reference Manual
// Revision: r1p5
// URL: http://infocenter.arm.com/help/index.jsp?topic=/com.arm.doc.ddi0183g/index.html
namespace uart::pl011 {

// This is where QEMU puts its emulated PL011.
constexpr zbi_dcfg_simple_t kQemuConfig{.mmio_phys = 0x09000000,
                                        .irq = 33,
                                        .flags = ZBI_KERNEL_DRIVER_IRQ_FLAGS_LEVEL_TRIGGERED |
                                                 ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_HIGH};

// We use expanded title (first clause in the Function column of the manual)
// rather than the acronym (Name column in the manual) for readability, except
// for the RS-232 standard acronyms and tx/rx for transmit/receive.

struct DataRegister : public hwreg::RegisterBase<DataRegister, uint16_t> {
  // 15:12 Reserved.
  DEF_BIT(11, overrun_error);
  DEF_BIT(10, break_error);
  DEF_BIT(9, parity_error);
  DEF_BIT(8, framing_error);
  DEF_FIELD(7, 0, data);

  static auto Get() { return hwreg::RegisterAddr<DataRegister>(0); }
};

struct FlagRegister : public hwreg::RegisterBase<FlagRegister, uint16_t> {
  // 15:9 Reserved, do not modify.
  DEF_BIT(8, ri);
  DEF_BIT(7, tx_fifo_empty);
  DEF_BIT(6, rx_fifo_full);
  DEF_BIT(5, tx_fifo_full);
  DEF_BIT(4, rx_fifo_empty);
  DEF_BIT(3, busy);
  DEF_BIT(2, dcd);
  DEF_BIT(1, dsr);
  DEF_BIT(0, cts);

  static auto Get() { return hwreg::RegisterAddr<FlagRegister>(0x18); }
};

struct ControlRegister : public hwreg::RegisterBase<ControlRegister, uint16_t> {
  DEF_BIT(15, cts_enable);
  DEF_BIT(14, rts_enable);
  DEF_BIT(13, out2);
  DEF_BIT(12, out1);
  DEF_BIT(11, rts);
  DEF_BIT(10, dtr);
  DEF_BIT(9, rx_enable);
  DEF_BIT(8, tx_enable);
  DEF_BIT(7, loopback_enable);
  // 6:3 Reserved, do not modify.
  DEF_BIT(2, sir_low_power);
  DEF_BIT(1, sir_enable);
  DEF_BIT(0, uart_enable);

  static auto Get() { return hwreg::RegisterAddr<ControlRegister>(0x30); }
};

struct InterruptFifoLevelSelectRegister
    : public hwreg::RegisterBase<InterruptFifoLevelSelectRegister, uint16_t> {
  // 15:6 Reserved, do not modify.

  enum class Level : uint8_t {
    ONE_EIGHTH = 0b000,
    ONE_QUARTER = 0b001,
    ONE_HALF = 0b010,
    THREE_QUARTERS = 0b011,
    SEVEN_EIGHTHS = 0b100,
  };

  DEF_ENUM_FIELD(Level, 5, 3, rx);
  DEF_ENUM_FIELD(Level, 2, 0, tx);

  static auto Get() { return hwreg::RegisterAddr<InterruptFifoLevelSelectRegister>(0x34); }
};

// The three interrupt-related registers have the same fields.  Neither
// inheritance nor template tricks seem to work with hwreg types, so rather
// than repeating the same fields in three types, just use one type with
// three different Get functions.
struct InterruptRegister : public hwreg::RegisterBase<InterruptRegister, uint16_t> {
  // 15:11 Reserved, do not modify.
  DEF_BIT(10, overrun_error);
  DEF_BIT(9, break_error);
  DEF_BIT(8, parity_error);
  DEF_BIT(7, framing_error);
  DEF_BIT(6, rx_timeout);
  DEF_BIT(5, tx);
  DEF_BIT(4, rx);
  DEF_BIT(3, dsr);
  DEF_BIT(2, dcd);
  DEF_BIT(1, cts);
  DEF_BIT(0, ri);

  static auto Get(uint32_t offset) { return hwreg::RegisterAddr<InterruptRegister>(offset); }
};

struct InterruptMaskSetClearRegister {
  static auto Get() { return InterruptRegister::Get(0x38); }
};

struct InterruptMaskedStatusRegister {
  static auto Get() { return InterruptRegister::Get(0x40); }
};

struct InterruptClearRegister {
  static auto Get() { return InterruptRegister::Get(0x44); }
};

struct LineControlRegister : public hwreg::RegisterBase<LineControlRegister, uint16_t> {
  DEF_FIELD(15, 8, reserved);
  DEF_BIT(7, enable_sticky_parity);
  DEF_FIELD(6, 5, word_length);
  DEF_BIT(4, fifo_enable);
  DEF_BIT(3, enable_two_stop_bits);
  DEF_BIT(2, enable_even_parity);
  DEF_BIT(1, enable_parity);
  DEF_BIT(0, enable_enable_send_break);

  static auto Get() { return hwreg::RegisterAddr<LineControlRegister>(0x2C); }
};

// The number of `IoSlots` used by this driver, determined by the last accessed register, see
// `LineControlRegister`. For unscaled MMIO, this corresponds to the size of the MMIO region
// from a provided base address.
static constexpr size_t kIoSlots = 0x2C + sizeof(uint16_t);

struct Driver : public DriverBase<Driver, ZBI_KERNEL_DRIVER_PL011_UART, zbi_dcfg_simple_t,
                                  IoRegisterType::kMmio8, kIoSlots> {
  using Base = DriverBase<Driver, ZBI_KERNEL_DRIVER_PL011_UART, zbi_dcfg_simple_t,
                          IoRegisterType::kMmio8, kIoSlots>;

  static constexpr auto kDevicetreeBindings =
      cpp20::to_array<std::string_view>({"arm,primecell", "arm,pl011"});
  static constexpr std::string_view kConfigName = "pl011";

  template <typename... Args>
  explicit Driver(Args&&... args) : Base(std::forward<Args>(args)...) {}

  using Base::TryMatch;

  static std::optional<Config<Driver>> TryMatch(std::string_view string) {
    if (string == "qemu") {
      return Config<Driver>(kQemuConfig);
    }
    return Base::TryMatch(string);
  }

  template <class IoProvider>
  void Init(IoProvider& io) {
    // Other line control settings were initialized by the hardware or the boot
    // loader and we just use them as they are.
    auto lcr = LineControlRegister::Get().ReadFrom(io.io());
    lcr.set_fifo_enable(true).WriteTo(io.io());

    auto cr = ControlRegister::Get().FromValue(0);
    cr.set_tx_enable(true).set_uart_enable(true).WriteTo(io.io());
  }

  template <class IoProvider>
  bool TxReady(IoProvider& io) {
    return FlagRegister::Get().ReadFrom(io.io()).tx_fifo_empty();
  }

  template <class IoProvider, typename It1, typename It2>
  auto Write(IoProvider& io, bool, It1 it, const It2& end) {
    DataRegister::Get().FromValue(0).set_data(*it).WriteTo(io.io());
    return ++it;
  }

  template <class IoProvider>
  std::optional<uint8_t> Read(IoProvider& io) {
    if (FlagRegister::Get().ReadFrom(io.io()).rx_fifo_empty()) {
      return {};
    }
    return DataRegister::Get().ReadFrom(io.io()).data();
  }

  template <class IoProvider>
  void EnableTxInterrupt(IoProvider& io, bool enable = true) {
    auto imscr = InterruptMaskSetClearRegister::Get().ReadFrom(io.io());
    imscr.set_tx(enable).WriteTo(io.io());
  }

  template <class IoProvider>
  void EnableRxInterrupt(IoProvider& io, bool enable = true) {
    auto imscr = InterruptMaskSetClearRegister::Get().ReadFrom(io.io());
    imscr.set_rx(enable).set_rx_timeout(enable).WriteTo(io.io());
  }

  template <class IoProvider, typename EnableInterruptCallback>
  void InitInterrupt(IoProvider& io, EnableInterruptCallback&& enable_interrupt_callback) {
    // Clear any pending interrupts.
    auto icr = InterruptClearRegister::Get().FromValue(0x3ff);
    icr.WriteTo(io.io());

    // Set the FIFO trigger levels to fastest trigger (1/8 capacity).
    auto fifo = InterruptFifoLevelSelectRegister::Get().FromValue(0);
    fifo.set_rx(InterruptFifoLevelSelectRegister::Level::ONE_EIGHTH)
        .set_tx(InterruptFifoLevelSelectRegister::Level::ONE_EIGHTH)
        .WriteTo(io.io());

    // Enable receive interrupts and then finally enable reception itself.
    // Transmit interrupts are enabled only when there is a blocked writer.
    EnableRxInterrupt(io);
    auto cr = ControlRegister::Get().ReadFrom(io.io());
    cr.set_rx_enable(true).WriteTo(io.io());

    enable_interrupt_callback();
  }

  template <class IoProvider, typename Lock, typename Waiter, typename Tx, typename Rx>
  void Interrupt(IoProvider& io, Lock& lock, Waiter& waiter, Tx&& tx, Rx&& rx) {
    auto misr = InterruptMaskedStatusRegister::Get().ReadFrom(io.io());
    if (misr.rx_timeout() || misr.rx()) {
      bool full = false;
      while (!full && !FlagRegister::Get().ReadFrom(io.io()).rx_fifo_empty()) {
        // Read the character if there's a place to put it.
        auto rx_irq = RxInterrupt(
            lock,  //
            [&]() { return DataRegister::Get().ReadFrom(io.io()).data(); },
            [&]() {
              // If the buffer is full, disable the receive interrupt instead
              // and stop checking.
              EnableRxInterrupt(io, false);
              full = true;
            });
        rx(rx_irq);
      }
    }
    if (misr.tx()) {
      auto tx_irq = TxInterrupt(lock, waiter, [&]() { EnableTxInterrupt(io, false); });
      tx(tx_irq);
    }
  }
};

}  // namespace uart::pl011

#endif  // LIB_UART_PL011_H_
