// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <endian.h>
#include <fcntl.h>
#include <fidl/fuchsia.hardware.ax88179/cpp/wire.h>
#include <fidl/fuchsia.hardware.ethernet/cpp/wire.h>
#include <lib/component/incoming/cpp/service_client.h>
#include <lib/fdio/cpp/caller.h>
#include <lib/fdio/fdio.h>
#include <lib/fdio/watcher.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/usb-virtual-bus-launcher/usb-virtual-bus-launcher.h>
#include <lib/zx/clock.h>
#include <zircon/device/ethernet.h>

#include <usb/cdc.h>
#include <usb/usb.h>
#include <zxtest/zxtest.h>

#include "asix-88179-regs.h"

namespace usb_ax88179 {
namespace {

using usb_virtual::BusLauncher;

namespace ethernet = fuchsia_hardware_ethernet;
namespace ax88179 = fuchsia_hardware_ax88179;

class UsbAx88179Test : public zxtest::Test {
 public:
  void SetUp() override {
    auto bus = BusLauncher::Create();
    ASSERT_OK(bus.status_value());
    bus_ = std::move(bus.value());
    ASSERT_NO_FATAL_FAILURE(InitUsbAx88179(&dev_path_, &test_function_path_));
  }

  void TearDown() override {
    ASSERT_OK(bus_->ClearPeripheralDeviceFunctions());

    ASSERT_OK(bus_->Disable());
  }

  void InitUsbAx88179(fbl::String* dev_path, fbl::String* test_function_path) {
    namespace usb_peripheral = fuchsia_hardware_usb_peripheral;
    using ConfigurationDescriptor =
        ::fidl::VectorView<fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor>;

    usb_peripheral::wire::DeviceDescriptor device_desc = {};
    device_desc.bcd_usb = htole16(0x0200);
    device_desc.b_max_packet_size0 = 64;
    device_desc.bcd_device = htole16(0x0100);
    device_desc.b_num_configurations = 1;

    usb_peripheral::wire::FunctionDescriptor usb_ax88179_desc = {
        .interface_class = USB_CLASS_COMM,
        .interface_subclass = USB_CDC_SUBCLASS_ETHERNET,
        .interface_protocol = 0,
    };

    device_desc.id_vendor = htole16(ASIX_VID);
    device_desc.id_product = htole16(AX88179_PID);
    std::vector<ConfigurationDescriptor> config_descs;
    std::vector<usb_peripheral::wire::FunctionDescriptor> function_descs;
    function_descs.push_back(usb_ax88179_desc);
    ConfigurationDescriptor config_desc;
    config_desc =
        fidl::VectorView<usb_peripheral::wire::FunctionDescriptor>::FromExternal(function_descs);
    config_descs.push_back(std::move(config_desc));
    ASSERT_OK(bus_->SetupPeripheralDevice(std::move(device_desc), std::move(config_descs)));

    fbl::unique_fd fd(openat(bus_->GetRootFd(), "class/ethernet", O_RDONLY));
    while (fdio_watch_directory(fd.get(), usb_virtual_bus::WaitForAnyFile, ZX_TIME_INFINITE,
                                dev_path) != ZX_ERR_STOP) {
    }
    *dev_path = fbl::String::Concat({fbl::String("class/ethernet/"), *dev_path});

    fd.reset(openat(bus_->GetRootFd(), "class/test-asix-function", O_RDONLY));
    while (fdio_watch_directory(fd.get(), usb_virtual_bus::WaitForAnyFile, ZX_TIME_INFINITE,
                                test_function_path) != ZX_ERR_STOP) {
    }
    *test_function_path =
        fbl::String::Concat({fbl::String("class/test-asix-function/"), *test_function_path});
  }

  void ConnectEthernetClient() {
    fdio_cpp::UnownedFdioCaller caller(bus_->GetRootFd());
    zx::result controller =
        component::ConnectAt<ethernet::Controller>(caller.directory(), dev_path_);
    ASSERT_OK(controller);
    zx::result endpoints = fidl::CreateEndpoints<ethernet::Device>();
    ASSERT_OK(endpoints);
    auto& [client, server] = endpoints.value();
    ASSERT_OK(fidl::WireCall(controller.value())->OpenSession(std::move(server)).status());
    ethernet_client_.Bind(std::move(client));

    // Get device information
    const fidl::WireResult info_result = ethernet_client_->GetInfo();
    ASSERT_OK(info_result);
    const fuchsia_hardware_ethernet::wire::Info& info = info_result.value().info;
    const fidl::WireResult fifos_result = ethernet_client_->GetFifos();
    ASSERT_OK(fifos_result);
    const fit::result fifos_response = fifos_result.value();
    ASSERT_TRUE(fifos_response.is_ok(), "%s", zx_status_get_string(fifos_response.error_value()));
    fuchsia_hardware_ethernet::wire::Fifos& fifos = fifos_response.value()->fifos;
    // Calculate optimal size of VMO, and set up RX and TX buffers.
    size_t optimal_vmo_size = (fifos.rx_depth * info.mtu) + (fifos.tx_depth * info.mtu);
    zx::vmo vmo;
    fzl::VmoMapper mapper;
    ASSERT_OK(mapper.CreateAndMap(optimal_vmo_size, ZX_VM_FLAG_PERM_READ | ZX_VM_FLAG_PERM_WRITE,
                                  nullptr, &vmo));
    auto set_io_buffer_result = ethernet_client_->SetIoBuffer(std::move(vmo));
    ASSERT_OK(set_io_buffer_result.status());

    rx_fifo_ = std::move(fifos.rx);
    tx_fifo_ = std::move(fifos.tx);
  }

  void StartDevice() {
    auto start_result = ethernet_client_->Start();
    ASSERT_OK(start_result.status());
  }

  void SetDeviceOnline() {
    fbl::unique_fd fd(openat(bus_->GetRootFd(), test_function_path_.c_str(), O_RDWR));
    zx::result test_client_end = fdio_cpp::FdioCaller(std::move(fd)).take_as<ax88179::Hooks>();
    ASSERT_OK(test_client_end.status_value());
    fidl::WireSyncClient test_client{std::move(*test_client_end)};

    // Ensure SIGNAL_STATUS is de-asserted before we set it.
    ASSERT_OK(ethernet_client_->GetStatus().status());

    auto online_result = test_client->SetOnline(true);
    ASSERT_OK(online_result.status());
    auto result = online_result->status;
    ASSERT_OK(result);
  }

  ethernet::wire::DeviceStatus GetDeviceStatus() {
    auto status_result = ethernet_client_->GetStatus();
    ZX_ASSERT(status_result.status() == ZX_OK);
    return status_result->device_status;
  }

  void ExpectStatusOnline() {
    // Two attempts, as SIGNAL_STATUS is triggered by both ethernet.cc starting
    // up and also by our fake function driver. As we can't control the delivery
    // order, confirm that we get to online eventually.
    for (int tries = 0; tries < 2; ++tries) {
      zx_signals_t pending;
      ASSERT_OK(rx_fifo_.wait_one(ethernet::wire::kSignalStatus, zx::time::infinite(), &pending));
      ASSERT_EQ((pending & ethernet::wire::kSignalStatus), ethernet::wire::kSignalStatus);
      if (GetDeviceStatus() & ethernet::wire::DeviceStatus::kOnline) {
        return;
      }
    }
    ADD_FATAL_FAILURE();
  }

 protected:
  std::optional<BusLauncher> bus_;
  fbl::String dev_path_;
  fbl::String test_function_path_;
  fidl::WireSyncClient<ethernet::Device> ethernet_client_;
  zx::fifo rx_fifo_;
  zx::fifo tx_fifo_;
};

TEST_F(UsbAx88179Test, SetupShutdownTest) { ASSERT_NO_FATAL_FAILURE(); }

TEST_F(UsbAx88179Test, OfflineByDefault) {
  ASSERT_NO_FATAL_FAILURE(ConnectEthernetClient());

  ASSERT_NO_FATAL_FAILURE(StartDevice());

  ASSERT_FALSE(GetDeviceStatus() & ethernet::wire::DeviceStatus::kOnline);
}

TEST_F(UsbAx88179Test, SetOnlineAfterStart) {
  ASSERT_NO_FATAL_FAILURE(ConnectEthernetClient());

  ASSERT_NO_FATAL_FAILURE(StartDevice());

  ASSERT_NO_FATAL_FAILURE(SetDeviceOnline());

  ASSERT_NO_FATAL_FAILURE(ExpectStatusOnline());
}

// This is for https://fxbug.dev/40786#c41.
TEST_F(UsbAx88179Test, SetOnlineBeforeStart) {
  ASSERT_NO_FATAL_FAILURE(ConnectEthernetClient());

  ASSERT_NO_FATAL_FAILURE(SetDeviceOnline());

  ASSERT_NO_FATAL_FAILURE(StartDevice());

  ASSERT_NO_FATAL_FAILURE(ExpectStatusOnline());
}

}  // namespace
}  // namespace usb_ax88179
