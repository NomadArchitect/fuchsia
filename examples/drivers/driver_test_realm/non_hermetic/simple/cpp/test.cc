// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/device-watcher/cpp/device-watcher.h>

#include <gtest/gtest.h>

// [START example]
TEST(SimpleDriverTestRealmTest, DriversExist) {
  zx::result channel = device_watcher::RecursiveWaitForFile("/dev/sys/test");
  ASSERT_EQ(channel.status_value(), ZX_OK);
}
// [END example]
