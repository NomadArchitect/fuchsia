// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ethernet.h"

#include <lib/async-loop/cpp/loop.h>

#include <zxtest/zxtest.h>

#include "test_util.h"

namespace ethernet_testing {

TEST(EthernetTest, BindTest) {
  EthernetTester tester;
  EXPECT_OK(eth::EthDev0::EthBind(nullptr, tester.parent().get()), "Bind failed");

  device_async_remove(tester.eth0()->zxdev());
  mock_ddk::ReleaseFlaggedDevices(tester.parent().get());
}

TEST(EthernetTest, DdkLifecycleTest) {
  EthernetTester tester;
  eth::EthDev0* eth(new eth::EthDev0(tester.parent().get()));
  EXPECT_OK(eth->AddDevice(), "AddDevice Failed");

  device_async_remove(eth->zxdev());
  mock_ddk::ReleaseFlaggedDevices(tester.parent().get());
}

class EthDev0ForTest : public eth::EthDev0 {
 public:
  explicit EthDev0ForTest(zx_device_t* parent) : eth::EthDev0(parent) {}
  using eth::EthDev0::DestroyAllEthDev;
};

class EthernetDeviceTest {
 public:
  explicit EthernetDeviceTest(uint32_t features) {
    tester.ethmac().SetFeatures(features);
    Initialize();
  }

  EthernetDeviceTest() : EthernetDeviceTest(0 /* features */) {}

  ~EthernetDeviceTest() {
    device_async_remove(edev0->zxdev());
    mock_ddk::ReleaseFlaggedDevices(tester.parent().get());

    loop_.Shutdown();
  }

  auto FidlClient() { return client_.sync(); }

  void Start() {
    {
      const fidl::WireResult result = FidlClient()->GetFifos();
      ASSERT_OK(result);
      const fit::result response = result.value();
      ASSERT_TRUE(response.is_ok(), "%s", zx_status_get_string(response.error_value()));
      fuchsia_hardware_ethernet::wire::Fifos& fifos = response.value()->fifos;
      tx_fifo_ = std::move(fifos.tx);
      EXPECT_TRUE(tx_fifo_.is_valid());
      rx_fifo_ = std::move(fifos.rx);
      rx_fifo_depth_ = fifos.rx_depth;
      tx_fifo_depth_ = fifos.tx_depth;
      EXPECT_TRUE(rx_fifo_.is_valid());
    }
    {
      ASSERT_OK(zx::vmo::create(2 * sizeof(ethernet_netbuf_t), 0, &buf_));
      auto result = FidlClient()->SetIoBuffer(std::move(buf_));
      ASSERT_OK(result.status());
      ASSERT_OK(result->status);
    }
    {
      auto result = FidlClient()->Start();
      ASSERT_OK(result.status());
      ASSERT_OK(result->status);
    }
  }

  zx::fifo& TransmitFifo() { return tx_fifo_; }

  zx::fifo& ReceiveFifo() { return rx_fifo_; }

  EthernetTester tester;
  // Unowned reference as mock-ddk owns the device through tester.parent().
  EthDev0ForTest* edev0;

  fbl::RefPtr<eth::EthDev> edev;

 private:
  void Initialize() {
    edev0 = new EthDev0ForTest(tester.parent().get());
    ASSERT_OK(edev0->AddDevice());

    edev = fbl::MakeRefCounted<eth::EthDev>(edev0);
    ASSERT_OK(edev->Init());

    auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_ethernet::Device>();
    ASSERT_OK(endpoints.status_value());

    ASSERT_OK(loop_.StartThread("ethernet-thread"));
    binding_ = fidl::BindServer(loop_.dispatcher(), std::move(endpoints->server), edev.get());
    client_.Bind(std::move(endpoints->client), loop_.dispatcher());
  }

  zx::fifo tx_fifo_;
  zx::fifo rx_fifo_;
  uint32_t rx_fifo_depth_;
  uint32_t tx_fifo_depth_;
  zx::vmo buf_;

  async::Loop loop_ = async::Loop(&kAsyncLoopConfigNeverAttachToThread);
  fidl::WireSharedClient<fuchsia_hardware_ethernet::Device> client_;
  std::optional<fidl::ServerBindingRef<fuchsia_hardware_ethernet::Device>> binding_;
};

TEST(EthernetTest, SetClientNameTest) {
  EthernetDeviceTest test;
  std::string name("ethtest");
  auto result = test.FidlClient()->SetClientName(fidl::StringView::FromExternal(name));
  ASSERT_OK(result.status());
  ASSERT_OK(result->status);
}

TEST(EthernetTest, GetInfoTest) {
  EthernetDeviceTest test;
  auto result = test.FidlClient()->GetInfo();
  ASSERT_OK(result.status());
  EXPECT_TRUE(test.tester.ethmac().TestInfo(&result->info));
}

TEST(EthernetTest, GetFifosTest) {
  EthernetDeviceTest test;
  const fidl::WireResult result = test.FidlClient()->GetFifos();
  ASSERT_OK(result);
  const fit::result response = result.value();
  ASSERT_TRUE(response.is_ok(), "%s", zx_status_get_string(response.error_value()));
  fuchsia_hardware_ethernet::wire::Fifos& fifos = response.value()->fifos;
  EXPECT_TRUE(fifos.rx != ZX_HANDLE_INVALID);
  EXPECT_TRUE(fifos.tx != ZX_HANDLE_INVALID);
}

TEST(EthernetTest, AddDeviceAsNotPromiscuous) {
  EthernetDeviceTest test;
  EXPECT_EQ(test.tester.ethmac().TestPromiscuous(), 0, "");
}

TEST(EthernetTest, SetPromiscuousModeTest) {
  EthernetDeviceTest test;

  {
    auto result = test.FidlClient()->SetPromiscuousMode(true);
    ASSERT_OK(result.status());
    ASSERT_OK(result->status);
    EXPECT_EQ(test.tester.ethmac().TestPromiscuous(), 1, "");
  }

  {
    auto result = test.FidlClient()->SetPromiscuousMode(false);
    ASSERT_OK(result.status());
    ASSERT_OK(result->status);
    EXPECT_EQ(test.tester.ethmac().TestPromiscuous(), 0, "");
  }
}

TEST(EthernetTest, ConfigMulticastAddMacTest) {
  EthernetDeviceTest test;
  {
    /* 1st bit should be 1 in multicast */
    fuchsia_hardware_ethernet::wire::MacAddress wrong_addr = {
        .octets = {0x00, 0xaa, 0xbb, 0xbb, 0xcc, 0xcc}};

    auto result = test.FidlClient()->ConfigMulticastAddMac(wrong_addr);
    ASSERT_OK(result.status());
    ASSERT_NOT_OK(result->status);
  }

  {
    fuchsia_hardware_ethernet::wire::MacAddress right_addr = {
        .octets = {0x01, 0xaa, 0xbb, 0xbb, 0xcc, 0xcc}};
    auto result = test.FidlClient()->ConfigMulticastAddMac(right_addr);
    ASSERT_OK(result.status());
    ASSERT_OK(result->status);
  }
}

TEST(EthernetTest, ConfigMulticastDeleteMacTest) {
  EthernetDeviceTest test;
  fuchsia_hardware_ethernet::wire::MacAddress addr = {
      .octets = {0xaa, 0xaa, 0xbb, 0xbb, 0xcc, 0xcc}};
  auto result = test.FidlClient()->ConfigMulticastDeleteMac(addr);
  ASSERT_OK(result.status());
  ASSERT_OK(result->status);
}

TEST(EthernetTest, ConfigMulticastSetPromiscuousModeTest) {
  EthernetDeviceTest test;
  {
    auto result = test.FidlClient()->ConfigMulticastSetPromiscuousMode(true);
    ASSERT_OK(result.status());
    ASSERT_OK(result->status);
  }

  {
    auto result = test.FidlClient()->ConfigMulticastSetPromiscuousMode(false);
    ASSERT_OK(result.status());
    ASSERT_OK(result->status);
  }
}

TEST(EthernetTest, ConfigMulticastTestFilterTest) {
  EthernetDeviceTest test;
  auto result = test.FidlClient()->ConfigMulticastTestFilter();
  ASSERT_OK(result.status());
  ASSERT_OK(result->status);
}

TEST(EthernetTest, DumpRegistersTest) {
  EthernetDeviceTest test;
  auto result = test.FidlClient()->DumpRegisters();
  ASSERT_OK(result.status());
  ASSERT_OK(result->status);
  EXPECT_TRUE(test.tester.ethmac().TestDump());
}

TEST(EthernetTest, SetIOBufferTest) {
  EthernetDeviceTest test;
  {
    auto result = test.FidlClient()->SetIoBuffer(zx::vmo());
    ASSERT_NOT_OK(result.status());
  }
  zx::vmo buf;
  ASSERT_OK(zx::vmo::create(2 * sizeof(ethernet_netbuf_t), 0, &buf));

  {
    auto result = test.FidlClient()->SetIoBuffer(std::move(buf));
    ASSERT_OK(result.status());
    ASSERT_OK(result->status);
  }
}

TEST(EthernetTest, StartTest) {
  EthernetDeviceTest test;
  // test bad state
  auto result = test.FidlClient()->Start();
  ASSERT_OK(result.status());
  ASSERT_NOT_OK(result->status);

  // test valid case
  ASSERT_NO_FATAL_FAILURE(test.Start());

  // test client interfaces
  EXPECT_TRUE(test.tester.ethmac().TestIfc());
}

TEST(EthernetTest, GetStatusTest) {
  EthernetDeviceTest test;

  // Start device.
  ASSERT_NO_FATAL_FAILURE(test.Start());

  // Set mock ethmac status.
  EXPECT_TRUE(test.tester.ethmac().SetStatus(1));

  // Verify FIFO is signalled.
  zx::fifo& rx = test.ReceiveFifo();
  zx_signals_t pending;
  ASSERT_OK(rx.wait_one(fuchsia_hardware_ethernet::wire::kSignalStatus, zx::time::infinite_past(),
                        &pending));
  ASSERT_EQ(pending & fuchsia_hardware_ethernet::wire::kSignalStatus,
            fuchsia_hardware_ethernet::wire::kSignalStatus);

  // Verify status.
  auto result = test.FidlClient()->GetStatus();
  ASSERT_OK(result.status());
  EXPECT_EQ(result->device_status, fuchsia_hardware_ethernet::wire::DeviceStatus::kOnline);

  // Status is cleared by reading through FIDL.
  ASSERT_STATUS(rx.wait_one(fuchsia_hardware_ethernet::wire::kSignalStatus,
                            zx::time::infinite_past(), &pending),
                ZX_ERR_TIMED_OUT);

  // Verify that updating status to the same value doesn't assert FIFO signals.
  EXPECT_TRUE(test.tester.ethmac().SetStatus(1));
  ASSERT_STATUS(rx.wait_one(fuchsia_hardware_ethernet::wire::kSignalStatus,
                            zx::time::infinite_past(), &pending),
                ZX_ERR_TIMED_OUT);
}

TEST(EthernetTest, SendTest) {
  EthernetDeviceTest test;

  // start device
  test.Start();

  // send packet through the fifo
  zx::fifo& tx = test.TransmitFifo();
  eth_fifo_entry_t entry = {
      .offset = 0,
      .length = 1,
      .flags = 0,
      .cookie = 0,
  };
  ASSERT_OK(tx.write(sizeof(entry), &entry, 1, nullptr));
  // wait for packet to be returned
  ASSERT_OK(tx.wait_one(ZX_FIFO_READABLE, zx::time::infinite(), nullptr));
  ASSERT_OK(tx.read(sizeof(entry), &entry, 1, nullptr));
  // check mock ethmac if packet was received
  EXPECT_TRUE(test.tester.ethmac().TestQueueTx());
}

TEST(EthernetTest, ReceiveTest) {
  EthernetDeviceTest test;

  // start device
  test.Start();

  // Queue buffer to receive fifo
  zx::fifo& rx = test.ReceiveFifo();
  eth_fifo_entry_t entry = {
      .offset = 0,
      .length = 1,
      .flags = 0,
      .cookie = 0,
  };
  ASSERT_OK(rx.write(sizeof(entry), &entry, 1, nullptr));

  // send packet through mock ethmac
  EXPECT_TRUE(test.tester.ethmac().TestRecv());

  // check if packet is received
  ASSERT_OK(rx.wait_one(ZX_FIFO_READABLE, zx::time::infinite(), nullptr));
  ASSERT_OK(rx.read(sizeof(entry), &entry, 1, nullptr));
}

TEST(EthernetTest, ListenStartTest) {
  EthernetDeviceTest test;

  // start device
  test.Start();

  // Set listen start
  auto result = test.FidlClient()->ListenStart();
  ASSERT_OK(result.status());
  ASSERT_OK(result->status);

  // send packet
  eth_fifo_entry_t entry = {
      .offset = 0,
      .length = 1,
      .flags = 0,
      .cookie = 0,
  };

  zx::fifo& rx = test.ReceiveFifo();
  ASSERT_OK(rx.write(sizeof(entry), &entry, 1, nullptr));

  zx::fifo& tx = test.TransmitFifo();
  ASSERT_OK(tx.write(sizeof(entry), &entry, 1, nullptr));

  // wait for the send to complete
  ASSERT_OK(tx.wait_one(ZX_FIFO_READABLE, zx::time::infinite(), nullptr));
  ASSERT_OK(tx.read(sizeof(entry), &entry, 1, nullptr));
  // check mock ethmac if packet was received
  EXPECT_TRUE(test.tester.ethmac().TestQueueTx());

  // check if it was echoed
  ASSERT_OK(rx.wait_one(ZX_FIFO_READABLE, zx::time::infinite(), nullptr));
  ASSERT_OK(rx.read(sizeof(entry), &entry, 1, nullptr));
}

TEST(EthernetTest, ListenStopTest) {
  EthernetDeviceTest test;
  auto result = test.FidlClient()->ListenStop();
  ASSERT_OK(result.status());
}

TEST(EthernetTest, StopTest) {
  EthernetDeviceTest test;
  test.Start();
  auto result = test.FidlClient()->Stop();
  ASSERT_OK(result.status());
}

// Tests that device implementations can call into the interface during a call
// to SetParam.
TEST(EthernetTest, ReentrantParamTest) {
  EthernetDeviceTest test;

  bool set_param_called = false;
  test.tester.ethmac().SetOnSetParamCallback([&set_param_called, &test]() {
    set_param_called = true;
    const uint8_t kData[] = {1, 2, 3};
    test.edev0->Recv(kData, sizeof(kData), 0);
  });

  auto result = test.FidlClient()->SetPromiscuousMode(true);
  ASSERT_OK(result.status());
  ASSERT_TRUE(set_param_called);
  test.tester.ethmac().SetOnSetParamCallback(nullptr);
}

class EthernetGetFeaturesTest
    : public zxtest::TestWithParam<std::pair<uint32_t, fuchsia_hardware_ethernet::wire::Features>> {
};

TEST_P(EthernetGetFeaturesTest, BanjoToFIDLTest) {
  const auto [banjo_features, fidl_features] = GetParam();

  EthernetDeviceTest test(banjo_features);
  auto result = test.FidlClient()->GetInfo();
  ASSERT_OK(result.status());
  EXPECT_EQ(result->info.features, fidl_features);
}

INSTANTIATE_TEST_SUITE_P(
    EthernetTests, EthernetGetFeaturesTest,
    zxtest::Values(
        std::make_tuple(0u, fuchsia_hardware_ethernet::wire::Features()),
        std::make_tuple(ETHERNET_FEATURE_WLAN, fuchsia_hardware_ethernet::wire::Features::kWlan),
        std::make_tuple(ETHERNET_FEATURE_SYNTH,
                        fuchsia_hardware_ethernet::wire::Features::kSynthetic),
        std::make_tuple(ETHERNET_FEATURE_WLAN | ETHERNET_FEATURE_SYNTH,
                        fuchsia_hardware_ethernet::wire::Features::kWlan |
                            fuchsia_hardware_ethernet::wire::Features::kSynthetic),
        std::make_tuple(ETHERNET_FEATURE_WLAN_AP,
                        fuchsia_hardware_ethernet::wire::Features::kWlanAp),
        std::make_tuple(ETHERNET_FEATURE_WLAN | ETHERNET_FEATURE_WLAN_AP,
                        fuchsia_hardware_ethernet::wire::Features::kWlan |
                            fuchsia_hardware_ethernet::wire::Features::kWlanAp),
        std::make_tuple(ETHERNET_FEATURE_SYNTH | ETHERNET_FEATURE_WLAN_AP,
                        fuchsia_hardware_ethernet::wire::Features::kSynthetic |
                            fuchsia_hardware_ethernet::wire::Features::kWlanAp),
        std::make_tuple(ETHERNET_FEATURE_WLAN | ETHERNET_FEATURE_SYNTH | ETHERNET_FEATURE_WLAN_AP,
                        fuchsia_hardware_ethernet::wire::Features::kWlan |
                            fuchsia_hardware_ethernet::wire::Features::kSynthetic |
                            fuchsia_hardware_ethernet::wire::Features::kWlanAp),
        std::make_tuple(std::numeric_limits<uint32_t>::max(),
                        fuchsia_hardware_ethernet::wire::Features::kWlan |
                            fuchsia_hardware_ethernet::wire::Features::kSynthetic |
                            fuchsia_hardware_ethernet::wire::Features::kWlanAp)));

}  // namespace ethernet_testing
