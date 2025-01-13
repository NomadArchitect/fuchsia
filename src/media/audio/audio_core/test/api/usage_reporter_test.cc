// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/media/cpp/fidl.h>

#include "src/media/audio/audio_core/shared/stream_usage.h"
#include "src/media/audio/audio_core/testing/integration/hermetic_audio_test.h"

using AudioCaptureUsage = fuchsia::media::AudioCaptureUsage;
using AudioRenderUsage = fuchsia::media::AudioRenderUsage;
using AudioRenderUsage2 = fuchsia::media::AudioRenderUsage2;
using AudioSampleFormat = fuchsia::media::AudioSampleFormat;

namespace media::audio::test {

namespace {
class FakeUsageWatcher : public fuchsia::media::UsageWatcher {
 public:
  explicit FakeUsageWatcher(TestFixture* fixture) : binding_(this) {
    fixture->AddErrorHandler(binding_, "FakeUsageWatcher");
  }

  fidl::InterfaceHandle<fuchsia::media::UsageWatcher> NewBinding() { return binding_.NewBinding(); }

  using Handler =
      std::function<void(fuchsia::media::Usage2 usage, fuchsia::media::UsageState usage_state)>;

  void SetNextHandler(Handler h) { next_handler_ = h; }

 private:
  void OnStateChanged(fuchsia::media::Usage _usage, fuchsia::media::UsageState usage_state,
                      OnStateChangedCallback callback) override {
    auto usage = ToFidlUsage2(_usage);
    if (next_handler_) {
      next_handler_(std::move(usage), std::move(usage_state));
      next_handler_ = nullptr;
    }
    callback();
  }

  fidl::Binding<fuchsia::media::UsageWatcher> binding_;
  Handler next_handler_;
};
}  // namespace

class UsageReporterTest : public HermeticAudioTest {
 protected:
  void SetUp() {
    HermeticAudioTest::SetUp();
    audio_core_->ResetInteractions();
  }

  struct Controller {
    Controller(TestFixture* fixture) : fake_watcher(fixture) {}

    fuchsia::media::UsageReporterPtr usage_reporter;
    FakeUsageWatcher fake_watcher;
  };

  std::unique_ptr<Controller> CreateController(AudioRenderUsage2 u) {
    auto c = std::make_unique<Controller>(this);
    realm().Connect(c->usage_reporter.NewRequest());
    AddErrorHandler(c->usage_reporter, "UsageReporter");

    c->usage_reporter->Watch(fuchsia::media::Usage::WithRenderUsage(*FromFidlRenderUsage2(u)),
                             c->fake_watcher.NewBinding());

    return c;
  }

  std::unique_ptr<Controller> CreateController(AudioCaptureUsage u) {
    auto c = std::make_unique<Controller>(this);
    realm().Connect(c->usage_reporter.NewRequest());
    AddErrorHandler(c->usage_reporter, "UsageReporter");
    c->usage_reporter->Watch(fuchsia::media::Usage::WithCaptureUsage(fidl::Clone(u)),
                             c->fake_watcher.NewBinding());

    return c;
  }

  void StartRendererWithUsage(AudioRenderUsage2 usage) {
    auto format = Format::Create<AudioSampleFormat::SIGNED_16>(1, 8000).value();  // arbitrary
    auto r = CreateAudioRenderer(format, 1024, usage);
    r->fidl()->PlayNoReply(0, 0);
  }

  void StartCapturerWithUsage(AudioCaptureUsage usage) {
    auto format = Format::Create<AudioSampleFormat::SIGNED_16>(1, 8000).value();  // arbitrary
    fuchsia::media::InputAudioCapturerConfiguration cfg;
    cfg.set_usage(usage);
    auto c = CreateAudioCapturer(
        format, 1024, fuchsia::media::AudioCapturerConfiguration::WithInput(std::move(cfg)));
    c->fidl()->StartAsyncCapture(1024);
  }
};

TEST_F(UsageReporterTest, RenderUsageInitialState) {
  auto c = CreateController(AudioRenderUsage2::MEDIA);

  fuchsia::media::Usage2 last_usage;
  fuchsia::media::UsageState last_state;
  c->fake_watcher.SetNextHandler(AddCallback(
      "OnStateChange",
      [&last_usage, &last_state](fuchsia::media::Usage2 usage, fuchsia::media::UsageState state) {
        last_usage = std::move(usage);
        last_state = std::move(state);
      }));

  // The initial callback happens immediately.
  ExpectCallbacks();
  EXPECT_TRUE(last_state.is_unadjusted());
  EXPECT_TRUE(last_usage.is_render_usage());
  EXPECT_EQ(last_usage.render_usage(), AudioRenderUsage2::MEDIA);
}

TEST_F(UsageReporterTest, RenderUsageDucked) {
  auto c = CreateController(AudioRenderUsage2::MEDIA);

  // The initial callback happens immediately.
  c->fake_watcher.SetNextHandler(AddCallback("OnStateChange InitialCall"));
  ExpectCallbacks();

  fuchsia::media::Usage2 last_usage;
  fuchsia::media::UsageState last_state;
  c->fake_watcher.SetNextHandler(AddCallback(
      "OnStateChange",
      [&last_usage, &last_state](fuchsia::media::Usage2 usage, fuchsia::media::UsageState state) {
        last_usage = std::move(usage);
        last_state = std::move(state);
      }));

  // Duck MEDIA when SYSTEM_AGENT is active.
  audio_core_->SetInteraction2(
      fuchsia::media::Usage2::WithRenderUsage(AudioRenderUsage2::SYSTEM_AGENT),
      fuchsia::media::Usage2::WithRenderUsage(AudioRenderUsage2::MEDIA),
      fuchsia::media::Behavior::DUCK);

  StartRendererWithUsage(AudioRenderUsage2::SYSTEM_AGENT);
  ExpectCallbacks();
  EXPECT_TRUE(last_state.is_ducked());
  EXPECT_TRUE(last_usage.is_render_usage());
  EXPECT_EQ(last_usage.render_usage(), AudioRenderUsage2::MEDIA);
}

TEST_F(UsageReporterTest, RenderUsageMuted) {
  auto c = CreateController(AudioRenderUsage2::MEDIA);

  // The initial callback happens immediately.
  c->fake_watcher.SetNextHandler(AddCallback("OnStateChange InitialCall"));
  ExpectCallbacks();

  fuchsia::media::Usage2 last_usage;
  fuchsia::media::UsageState last_state;
  c->fake_watcher.SetNextHandler(AddCallback(
      "OnStateChange",
      [&last_usage, &last_state](fuchsia::media::Usage2 usage, fuchsia::media::UsageState state) {
        last_usage = std::move(usage);
        last_state = std::move(state);
      }));

  // Mute MEDIA when SYSTEM_AGENT is active.
  audio_core_->SetInteraction2(
      fuchsia::media::Usage2::WithRenderUsage(AudioRenderUsage2::SYSTEM_AGENT),
      fuchsia::media::Usage2::WithRenderUsage(AudioRenderUsage2::MEDIA),
      fuchsia::media::Behavior::MUTE);

  StartRendererWithUsage(AudioRenderUsage2::SYSTEM_AGENT);
  ExpectCallbacks();
  EXPECT_TRUE(last_state.is_muted());
  EXPECT_TRUE(last_usage.is_render_usage());
  EXPECT_EQ(last_usage.render_usage(), AudioRenderUsage2::MEDIA);
}

TEST_F(UsageReporterTest, CaptureUsageInitialState) {
  auto c = CreateController(AudioCaptureUsage::COMMUNICATION);

  fuchsia::media::Usage2 last_usage;
  fuchsia::media::UsageState last_state;
  c->fake_watcher.SetNextHandler(AddCallback(
      "OnStateChange",
      [&last_usage, &last_state](fuchsia::media::Usage2 usage, fuchsia::media::UsageState state) {
        last_usage = std::move(usage);
        last_state = std::move(state);
      }));

  // The initial callback happens immediately.
  ExpectCallbacks();
  EXPECT_TRUE(last_state.is_unadjusted());
  EXPECT_TRUE(last_usage.is_capture_usage());
  EXPECT_EQ(last_usage.capture_usage(), AudioCaptureUsage::COMMUNICATION);
}

TEST_F(UsageReporterTest, CaptureUsageDucked) {
  auto c = CreateController(AudioCaptureUsage::COMMUNICATION);

  // The initial callback happens immediately.
  c->fake_watcher.SetNextHandler(AddCallback("OnStateChange InitialCall"));
  ExpectCallbacks();

  fuchsia::media::Usage2 last_usage;
  fuchsia::media::UsageState last_state;
  c->fake_watcher.SetNextHandler(AddCallback(
      "OnStateChange",
      [&last_usage, &last_state](fuchsia::media::Usage2 usage, fuchsia::media::UsageState state) {
        last_usage = std::move(usage);
        last_state = std::move(state);
      }));

  // Duck COMMUNICATION when SYSTEM_AGENT is active.
  audio_core_->SetInteraction2(
      fuchsia::media::Usage2::WithCaptureUsage(AudioCaptureUsage::SYSTEM_AGENT),
      fuchsia::media::Usage2::WithCaptureUsage(AudioCaptureUsage::COMMUNICATION),
      fuchsia::media::Behavior::DUCK);

  StartCapturerWithUsage(AudioCaptureUsage::SYSTEM_AGENT);
  ExpectCallbacks();
  EXPECT_TRUE(last_state.is_ducked());
  EXPECT_TRUE(last_usage.is_capture_usage());
  EXPECT_EQ(last_usage.capture_usage(), AudioCaptureUsage::COMMUNICATION);
}

TEST_F(UsageReporterTest, CaptureUsageMuted) {
  auto c = CreateController(AudioCaptureUsage::COMMUNICATION);

  // The initial callback happens immediately.
  c->fake_watcher.SetNextHandler(AddCallback("OnStateChange InitialCall"));
  ExpectCallbacks();

  fuchsia::media::Usage2 last_usage;
  fuchsia::media::UsageState last_state;
  c->fake_watcher.SetNextHandler(AddCallback(
      "OnStateChange",
      [&last_usage, &last_state](fuchsia::media::Usage2 usage, fuchsia::media::UsageState state) {
        last_usage = std::move(usage);
        last_state = std::move(state);
      }));

  // Mute COMMUNICATION when SYSTEM_AGENT is active.
  audio_core_->SetInteraction2(
      fuchsia::media::Usage2::WithCaptureUsage(AudioCaptureUsage::SYSTEM_AGENT),
      fuchsia::media::Usage2::WithCaptureUsage(AudioCaptureUsage::COMMUNICATION),
      fuchsia::media::Behavior::MUTE);

  StartCapturerWithUsage(AudioCaptureUsage::SYSTEM_AGENT);
  ExpectCallbacks();
  EXPECT_TRUE(last_state.is_muted());
  EXPECT_TRUE(last_usage.is_capture_usage());
  EXPECT_EQ(last_usage.capture_usage(), AudioCaptureUsage::COMMUNICATION);
}

}  // namespace media::audio::test
