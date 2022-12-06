// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <usb-monitor-util/usb-monitor-util.h>
#include <zxtest/zxtest.h>

namespace {
TEST(UsbMonitorUtilTest, StartStop) {
  UsbMonitor test_monitor;
  ASSERT_FALSE(test_monitor.Started());
  test_monitor.Start();
  ASSERT_TRUE(test_monitor.Started());
  test_monitor.Stop();
  ASSERT_FALSE(test_monitor.Started());
}

TEST(UsbMonitorUtilTest, StartAddRecordStop) {
  UsbMonitor test_monitor;
  test_monitor.Start();
  test_monitor.AddRecord({});
  const UsbMonitorStats test_record = test_monitor.GetStats();
  ASSERT_EQ(1u, test_record.num_records, "One record should have been added");
  ASSERT_TRUE(test_monitor.Started());
  test_monitor.Stop();
  ASSERT_FALSE(test_monitor.Started());
}

}  // namespace
