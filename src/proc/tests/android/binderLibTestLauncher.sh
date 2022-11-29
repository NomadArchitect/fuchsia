#!/system/bin/sh
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -e

# Start servicemanager in the background, which is the name server for binder objects.
servicemanager &

function cleanup {
    # Kill servicemanager, as it will never exit on its own.
    kill -9 `jobs -p`
}
trap cleanup EXIT

GTEST_EXCLUDE_FILTER=""
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.CheckNoHeaderMappedInUser"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.SchedPolicySet"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.InheritRt"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.GotSid"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.ThreadPoolAvailableThreads"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.HangingServices"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.TooManyFdsFlattenable"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:ServiceNotifications.Unregister"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibRpcTest*"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest/BinderLibRpcTestP.*"

# Flaking tests fxb/101486
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.BinderCallContextGuard"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.CallBack"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.DeathNotificationMultiple"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.IndirectGetId2"
GTEST_EXCLUDE_FILTER="$GTEST_EXCLUDE_FILTER:BinderLibTest.IndirectGetId3"

# Change directory to the temporary directory, as the expected /data/local/tmp
# directory doesn't exist.
cd /data/tmp
# Start the actual test.
/vendor/data/nativetest64/binderLibTest/binderLibTest "--gtest_filter=-${GTEST_EXCLUDE_FILTER}"
