// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zxtest/zxtest.h>

namespace {

TEST(BasicTest, BasicFailure) { ASSERT_TRUE(false); }

}  // namespace
