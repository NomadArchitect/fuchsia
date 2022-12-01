// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/services/mixer/mix/simple_packet_queue_producer_stage.h"

#include <lib/zx/time.h>

#include <map>
#include <memory>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/media/audio/lib/format2/fixed.h"
#include "src/media/audio/lib/format2/format.h"
#include "src/media/audio/services/common/testing/test_fence.h"
#include "src/media/audio/services/mixer/mix/mix_job_context.h"
#include "src/media/audio/services/mixer/mix/packet_view.h"
#include "src/media/audio/services/mixer/mix/testing/defaults.h"
#include "src/media/audio/services/mixer/mix/testing/fake_pipeline_thread.h"

namespace media_audio {
namespace {

using ::fuchsia_audio::SampleType;
using CommandQueue = SimplePacketQueueProducerStage::CommandQueue;
using PushPacketCommand = SimplePacketQueueProducerStage::PushPacketCommand;
using ReleasePacketsCommand = SimplePacketQueueProducerStage::ReleasePacketsCommand;
using ::testing::ElementsAre;

const Format kFormat = Format::CreateOrDie({SampleType::kFloat32, 2, 48000});

class SimplePacketQueueProducerStageTest : public ::testing::Test {
 public:
  SimplePacketQueueProducerStageTest(std::shared_ptr<CommandQueue> command_queue = nullptr)
      : command_queue_(command_queue),
        packet_queue_producer_stage_({
            .format = kFormat,
            .reference_clock = DefaultUnreadableClock(),
            .initial_thread = std::make_shared<FakePipelineThread>(1),
            .command_queue = command_queue,
            .underflow_reporter = [this](auto duration) { ReportUnderflow(duration); },
        }) {
    packet_queue_producer_stage_.UpdatePresentationTimeToFracFrame(
        DefaultPresentationTimeToFracFrame(kFormat));
  }

  const void* PushPacket(uint32_t packet_id, int64_t start = 0, int64_t length = 1) {
    auto& packet = NewPacket(packet_id, start, length);
    packet_queue_producer_stage_.push(packet.view, packet.fence.Take());
    return packet.payload.data();
  }

  SimplePacketQueueProducerStage& packet_queue_producer_stage() {
    return packet_queue_producer_stage_;
  }

  const std::vector<uint32_t>& released_packets() {
    for (auto& [id, packet] : packets_) {
      if (!packet.released && packet.fence.Done()) {
        released_packets_.push_back(id);
        packet.released = true;
      }
    }
    return released_packets_;
  }

  void SetOnUnderflow(std::function<void(zx::duration)> f) { on_underflow_ = f; }

 protected:
  struct Packet {
    explicit Packet(int64_t start, int64_t length)
        : payload(length, 0.0f),
          view({
              .format = kFormat,
              .start_frame = Fixed(start),
              .frame_count = length,
              .payload = payload.data(),
          }) {}

    std::vector<float> payload;
    PacketView view;
    TestFence fence;
    bool released = false;
  };

  Packet& NewPacket(uint32_t packet_id, int64_t start, int64_t length) {
    auto [it, inserted] = packets_.try_emplace(packet_id, start, length);
    FX_CHECK(inserted) << "duplicate packet with id " << packet_id;
    return it->second;
  }

  void ReportUnderflow(zx::duration duration) {
    if (on_underflow_) {
      on_underflow_(duration);
    }
  }

  std::shared_ptr<CommandQueue> command_queue() const { return command_queue_; }

 private:
  std::shared_ptr<CommandQueue> command_queue_;
  SimplePacketQueueProducerStage packet_queue_producer_stage_;
  std::map<int32_t, Packet> packets_;  // ordered map so iteration is deterministic
  std::vector<uint32_t> released_packets_;
  std::function<void(zx::duration)> on_underflow_;
};

TEST_F(SimplePacketQueueProducerStageTest, Push) {
  SimplePacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push packet.
  PushPacket(0);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());
}

TEST_F(SimplePacketQueueProducerStageTest, Read) {
  SimplePacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push some packets.
  const void* payload_0 = PushPacket(0, 0, 20);
  const void* payload_1 = PushPacket(1, 20, 20);
  const void* payload_2 = PushPacket(2, 40, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Now pop the packets one by one.
  {
    // Packet #0:
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(0), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start_frame());
    EXPECT_EQ(20, buffer->frame_count());
    EXPECT_EQ(20, buffer->end_frame());
    EXPECT_EQ(payload_0, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));

  {
    // Packet #1:
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(20), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(20, buffer->start_frame());
    EXPECT_EQ(20, buffer->frame_count());
    EXPECT_EQ(40, buffer->end_frame());
    EXPECT_EQ(payload_1, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1));

  {
    // Packet #2:
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(40), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(40, buffer->start_frame());
    EXPECT_EQ(20, buffer->frame_count());
    EXPECT_EQ(60, buffer->end_frame());
    EXPECT_EQ(payload_2, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2));
}

TEST_F(SimplePacketQueueProducerStageTest, ReadMultiplePerPacket) {
  SimplePacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  const auto bytes_per_frame = packet_queue.format().bytes_per_frame();

  // Push packet.
  const void* payload = PushPacket(0, 0, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Read the first 10 bytes of the packet.
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(0), 10);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start_frame());
    EXPECT_EQ(10, buffer->frame_count());
    EXPECT_EQ(10, buffer->end_frame());
    EXPECT_EQ(payload, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Read the next 10 bytes of the packet.
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(10), 10);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(10, buffer->start_frame());
    EXPECT_EQ(10, buffer->frame_count());
    EXPECT_EQ(20, buffer->end_frame());
    EXPECT_EQ(static_cast<const uint8_t*>(payload) + 10 * bytes_per_frame, buffer->payload());
    EXPECT_FALSE(packet_queue.empty());
  }
  // Now that the packet has been fully consumed, it should be released.
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));
}

TEST_F(SimplePacketQueueProducerStageTest, ReadNotFullyConsumed) {
  SimplePacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push some packets.
  PushPacket(0, 0, 20);
  PushPacket(1, 20, 20);
  PushPacket(2, 40, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Pop, consume 0/20 bytes.
    auto buffer = packet_queue.Read(DefaultCtx(), Fixed(0), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start_frame());
    EXPECT_EQ(20, buffer->frame_count());
    buffer->set_frames_consumed(0);
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Pop, consume 5/20 bytes.
    auto buffer = packet_queue.Read(DefaultCtx(), Fixed(0), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start_frame());
    EXPECT_EQ(20, buffer->frame_count());
    buffer->set_frames_consumed(5);
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Pop again, consume 10/15 bytes.
    auto buffer = packet_queue.Read(DefaultCtx(), Fixed(5), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(5, buffer->start_frame());
    EXPECT_EQ(15, buffer->frame_count());
    buffer->set_frames_consumed(10);
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Pop again, this time consume it fully.
    auto buffer = packet_queue.Read(DefaultCtx(), Fixed(15), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(15, buffer->start_frame());
    EXPECT_EQ(5, buffer->frame_count());
    buffer->set_frames_consumed(5);
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));
}

TEST_F(SimplePacketQueueProducerStageTest, ReadSkipsOverPacket) {
  SimplePacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push some packets.
  PushPacket(0, 0, 20);
  PushPacket(1, 20, 20);
  PushPacket(2, 40, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  {
    // Ask for packet 0.
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(0), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(0, buffer->start_frame());
    EXPECT_EQ(20, buffer->frame_count());
    EXPECT_EQ(20, buffer->end_frame());
  }
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));

  {
    // Ask for packet 2, skipping over packet 1.
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(40), 20);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(40, buffer->start_frame());
    EXPECT_EQ(20, buffer->frame_count());
    EXPECT_EQ(60, buffer->end_frame());
  }
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2));
}

TEST_F(SimplePacketQueueProducerStageTest, Advance) {
  SimplePacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Push some packets.
  PushPacket(0, 0, 20);
  PushPacket(1, 20, 20);
  PushPacket(2, 40, 20);
  PushPacket(3, 60, 20);
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // The last frame in the first packet is 19.
  // Verify that advancing to that frame does not release the first packet.
  packet_queue.Advance(DefaultCtx(), Fixed(19));
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Advance again with the same frame to verify it is idempotent.
  packet_queue.Advance(DefaultCtx(), Fixed(19));
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  // Now advance to the next packet.
  packet_queue.Advance(DefaultCtx(), Fixed(20));
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0));

  // Now advance beyond packet 1 and 2 in one go (until just before packet 3 should be released).
  packet_queue.Advance(DefaultCtx(), Fixed(79));
  EXPECT_FALSE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2));

  // Finally advance past the end of all packets.
  packet_queue.Advance(DefaultCtx(), Fixed(1000));
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2, 3));
}

TEST_F(SimplePacketQueueProducerStageTest, ReportUnderflow) {
  SimplePacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_TRUE(released_packets().empty());

  std::vector<zx::duration> reported_underflows;
  SetOnUnderflow(
      [&reported_underflows](zx::duration duration) { reported_underflows.push_back(duration); });

  // This test uses 48k fps, so 10ms = 480 frames.
  constexpr int64_t kPacketSize = 480;
  constexpr int64_t kFrameAt05ms = kPacketSize / 2;
  constexpr int64_t kFrameAt15ms = kPacketSize + kPacketSize / 2;
  constexpr int64_t kFrameAt20ms = 2 * kPacketSize;

  {
    // Advance to t=20ms.
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(0), 2 * kPacketSize);
    EXPECT_FALSE(buffer);
    EXPECT_TRUE(reported_underflows.empty());
  }

  // Push two packets, one that fully underflows and one that partially underflows.
  PushPacket(0, kFrameAt05ms, kPacketSize);
  PushPacket(1, kFrameAt15ms, kPacketSize);

  {
    // The next `Read` advances to t=25ms, returning part of the queued packet.
    reported_underflows.clear();
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(kFrameAt20ms), kPacketSize);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(kFrameAt20ms, buffer->start_frame());
    EXPECT_EQ(kPacketSize / 2, buffer->frame_count());
    EXPECT_THAT(reported_underflows, ElementsAre(zx::msec(15), zx::msec(5)));
  }
  // After packet is released, the queue should be empty.
  EXPECT_TRUE(packet_queue.empty());
  EXPECT_THAT(released_packets(), ElementsAre(0, 1));
}

class SimplePacketQueueProducerStageTestWithCommandQueue
    : public SimplePacketQueueProducerStageTest {
 public:
  SimplePacketQueueProducerStageTestWithCommandQueue()
      : SimplePacketQueueProducerStageTest(std::make_shared<CommandQueue>()) {}

  const void* PushPacket(int64_t segment_id, uint32_t packet_id, int64_t start = 0,
                         int64_t length = 1) {
    auto& packet = NewPacket(packet_id, start, length);
    command_queue()->push(PushPacketCommand{
        .packet = packet.view,
        .segment_id = segment_id,
        .fence = packet.fence.Take(),
    });
    return packet.payload.data();
  }

  void ReleasePackets(int64_t before_segment_id) {
    command_queue()->push(ReleasePacketsCommand{.before_segment_id = before_segment_id});
  }

 private:
  std::shared_ptr<CommandQueue> command_queue_;
};

TEST_F(SimplePacketQueueProducerStageTestWithCommandQueue, ReleaseAndRead) {
  SimplePacketQueueProducerStage& packet_queue = packet_queue_producer_stage();
  EXPECT_TRUE(released_packets().empty());

  // Send PushPacket commands.
  PushPacket(0, 0, 0, 20);
  PushPacket(0, 1, 20, 20);
  PushPacket(1, 2, 40, 20);
  EXPECT_TRUE(released_packets().empty());

  // Release all packets before segment_id=2, which is all above packets.
  ReleasePackets(2);
  {
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(0), 60);
    ASSERT_FALSE(buffer);
  }
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2));

  // Send more PushPacket commands.
  PushPacket(1, 3, 60, 20);
  PushPacket(2, 4, 80, 20);

  // Packet 3 should be released immediately, so Read should return packet 4.
  {
    const auto buffer = packet_queue.Read(DefaultCtx(), Fixed(60), 40);
    ASSERT_TRUE(buffer);
    EXPECT_EQ(80, buffer->start_frame());
  }

  // Now all packets are released.
  EXPECT_THAT(released_packets(), ElementsAre(0, 1, 2, 3, 4));
}

}  // namespace
}  // namespace media_audio
