// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_SPI_DRIVERS_SPI_SPI_CHILD_H_
#define SRC_DEVICES_SPI_DRIVERS_SPI_SPI_CHILD_H_

#include <fidl/fuchsia.hardware.spi/cpp/wire.h>
#include <fuchsia/hardware/spi/cpp/banjo.h>
#include <fuchsia/hardware/spiimpl/cpp/banjo.h>
#include <lib/sys/component/cpp/outgoing_directory.h>

#include <variant>

#include <ddktl/device.h>

// This header defines three classes: SpiChild, SpiFidlChild, and SpiBanjoChild. They are arranged
// in the node topology as follows:
//
//   spi --> SpiDevice (from spi.h)
//     spi-0-0 --> SpiChild
//       spi-fidl-0-0 --> SpiFidlChild
//       spi-banjo-0-0 --> SpiBanjoChild
//
// SpiDevice and SpiChild implement the actual SPI logic; SpiFidlChild and SpiBanjoChild serve the
// fuchsia.hardware.spi protocols over FIDL and Banjo, respectively, but delegate to their SpiChild
// parent for the SPI operations. SpiChild also exposes a /dev/class/spi entry.

namespace spi {

class SpiDevice;

class SpiChild;
using SpiChildType = ddk::Device<SpiChild, ddk::Messageable<fuchsia_hardware_spi::Device>::Mixin,
                                 ddk::Unbindable, ddk::Openable, ddk::Closable>;

class SpiChild : public SpiChildType {
 public:
  SpiChild(zx_device_t* parent, ddk::SpiImplProtocolClient spi, uint32_t chip_select,
           bool has_siblings)
      : SpiChildType(parent), spi_(spi), cs_(chip_select), has_siblings_(has_siblings) {}

  void DdkUnbind(ddk::UnbindTxn txn);
  void DdkRelease();

  void TransmitVector(TransmitVectorRequestView request,
                      TransmitVectorCompleter::Sync& completer) override;
  void ReceiveVector(ReceiveVectorRequestView request,
                     ReceiveVectorCompleter::Sync& completer) override;
  void ExchangeVector(ExchangeVectorRequestView request,
                      ExchangeVectorCompleter::Sync& completer) override;

  void RegisterVmo(RegisterVmoRequestView request, RegisterVmoCompleter::Sync& completer) override;
  void UnregisterVmo(UnregisterVmoRequestView request,
                     UnregisterVmoCompleter::Sync& completer) override;

  void Transmit(TransmitRequestView request, TransmitCompleter::Sync& completer) override;
  void Receive(ReceiveRequestView request, ReceiveCompleter::Sync& completer) override;
  void Exchange(ExchangeRequestView request, ExchangeCompleter::Sync& completer) override;

  void CanAssertCs(CanAssertCsCompleter::Sync& completer) override;
  void AssertCs(AssertCsCompleter::Sync& completer) override;
  void DeassertCs(DeassertCsCompleter::Sync& completer) override;

  zx_status_t SpiTransmit(const uint8_t* txdata_list, size_t txdata_count);
  zx_status_t SpiReceive(uint32_t size, uint8_t* out_rxdata_list, size_t rxdata_count,
                         size_t* out_rxdata_actual);
  zx_status_t SpiExchange(const uint8_t* txdata_list, size_t txdata_count, uint8_t* out_rxdata_list,
                          size_t rxdata_count, size_t* out_rxdata_actual);

  void Bind(async_dispatcher_t* dispatcher,
            fidl::ServerEnd<fuchsia_hardware_spi::Device> server_end);

  zx_status_t DdkOpen(zx_device_t** dev_out, uint32_t flags);
  zx_status_t DdkClose(uint32_t flags);

 private:
  const ddk::SpiImplProtocolClient spi_;
  const uint32_t cs_;
  // False if this child is the only device on the bus.
  const bool has_siblings_;

  using Binding = struct {
    fidl::ServerBindingRef<fuchsia_hardware_spi::Device> binding;
    std::optional<ddk::UnbindTxn> unbind_txn;
  };
  // Tri-state exclusive ownership:
  //
  // - std::nullopt if unowned.
  // - std::monostate if owned by DdkOpen.
  // - Binding if owned by `SpiFidlChild`.
  std::optional<std::variant<std::monostate, Binding>> owner_;
};

class SpiFidlChild;
using SpiFidlChildType = ddk::Device<SpiFidlChild>;

// An SPI child device that serves the fuchsia.hardware.spi/Device FIDL
// protocol. Note that while SpiChild also serves this protocol, it does not
// expose it in its outgoing directory for its children to use, while
// SpiFidlChild does. Otherwise, it simply delegates all its FIDL methods to
// SpiChild.
//
// See SpiBanjoChild for the corresponding Banjo sibling device.
class SpiFidlChild : public SpiFidlChildType {
 public:
  SpiFidlChild(zx_device_t* parent, SpiChild* spi, async_dispatcher_t* dispatcher);

  void DdkRelease();

  zx_status_t ServeOutgoingDirectory(fidl::ServerEnd<fuchsia_io::Directory> server_end);

 private:
  // SpiChild is the parent of SpiFidlChild so it is guaranteed to outlive it,
  // and this pointer will always remain valid.
  SpiChild* spi_;
  component::OutgoingDirectory outgoing_;
};

class SpiBanjoChild;
using SpiBanjoChildType = ddk::Device<SpiBanjoChild, ddk::GetProtocolable>;

class SpiBanjoChild : public SpiBanjoChildType, public ddk::SpiProtocol<SpiBanjoChild> {
 public:
  SpiBanjoChild(zx_device_t* parent, SpiChild* spi) : SpiBanjoChildType(parent), spi_(spi) {}

  void DdkRelease() { delete this; }
  zx_status_t DdkGetProtocol(uint32_t proto_id, void* out_protocol);

  // Banjo implementation
  zx_status_t SpiTransmit(const uint8_t* txdata_list, size_t txdata_count);
  zx_status_t SpiReceive(uint32_t size, uint8_t* out_rxdata_list, size_t rxdata_count,
                         size_t* out_rxdata_actual);
  zx_status_t SpiExchange(const uint8_t* txdata_list, size_t txdata_count, uint8_t* out_rxdata_list,
                          size_t rxdata_count, size_t* out_rxdata_actual);

 private:
  // SpiChild is the parent of SpiBanjoChild so it is guaranteed to outlive it,
  // and this pointer will always remain valid.
  SpiChild* spi_;
};

}  // namespace spi

#endif  // SRC_DEVICES_SPI_DRIVERS_SPI_SPI_CHILD_H_
