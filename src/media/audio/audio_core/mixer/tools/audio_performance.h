// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_AUDIO_CORE_MIXER_TOOLS_AUDIO_PERFORMANCE_H_
#define SRC_MEDIA_AUDIO_AUDIO_CORE_MIXER_TOOLS_AUDIO_PERFORMANCE_H_

#include <perftest/results.h>

#include "src/media/audio/audio_core/mixer/mixer.h"

namespace media::audio::tools {

// TODO(https://fxbug.dev/42127933): Consider migrating to google/benchmark

// The AudioPerformance class profiles the performance of the Mixer, Gain and OutputProducer
// classes. These micro-benchmark tests use *zx::clock::get_monotonic()* to measure the time
// required for a Mixer/Gain or OutputProducer to execute Mix() or ProduceOutput() respectively,
// generating 64k output frames. It also profiles the time required for initial mixer creation.
//
// The aggregated results displayed for each permutation of parameters represent the time consumed
// *per-call* or *per-creation*, although to determine a relatively reliable Mean we run the
// micro-benchmarks many tens or even hundreds of times.
//
// As is often the case with performance profiling, one generally should not directly compare
// results from different machines; one would use profiling functionality primarily to gain a sense
// of "before versus after" with respect to a specific change affecting the mixer pipeline.
class AudioPerformance {
 public:
  // class is static only - prevent attempts to instantiate it
  AudioPerformance() = delete;

  enum class GainType : uint8_t { Mute, Unity, Scaled, Ramped };
  enum class OutputSourceRange : uint8_t { Silence, OutOfRange, Normal };

  struct Limits {
    zx::duration duration_per_config;
    size_t runs_per_config;
    size_t min_runs_per_config;
  };

  struct MixerConfig {
    Mixer::Resampler sampler_type;
    int32_t num_input_chans;
    int32_t num_output_chans;
    int32_t source_rate;
    int32_t dest_rate;
    fuchsia::media::AudioSampleFormat sample_format;
    GainType gain_type;  // ProfileMixing() only
    bool accumulate;     // ProfileMixing() only

    bool operator==(const MixerConfig& other) const;
    bool operator!=(const MixerConfig& other) const;

    std::string ToStringForCreate() const;
    std::string ToStringForMixing() const;
    std::string ToPerftestFormat() const;
    std::string ToPerftestFormatForCreation() const;
    std::string ToPerftestFormatForMixing() const;
  };

  struct OutputProducerConfig {
    fuchsia::media::AudioSampleFormat sample_format;
    OutputSourceRange output_range;
    int32_t num_chans;

    bool operator==(const OutputProducerConfig& other) const;
    bool operator!=(const OutputProducerConfig& other) const;

    std::string ToString() const;
    std::string ToPerftestFormat() const;
  };

  static void ProfileMixerCreation(const std::vector<MixerConfig>& configs, const Limits& limits,
                                   perftest::ResultsSet* results);
  static void ProfileMixing(const std::vector<MixerConfig>& configs, const Limits& limits,
                            perftest::ResultsSet* results);
  static void ProfileMixOutput(const std::vector<OutputProducerConfig>& configs,
                               const Limits& limits, perftest::ResultsSet* results);

 private:
  static void DisplayMixerCreationLegend();
  static void DisplayMixerCreationColumnHeader();
  static void ProfileMixerCreation(const MixerConfig& cfg, const Limits& limits,
                                   perftest::ResultsSet* results);

  static void DisplayMixingLegend();
  static void DisplayMixingColumnHeader();
  template <fuchsia::media::AudioSampleFormat SampleFormat>
  static void ProfileMixing(const MixerConfig& cfg, const Limits& limits,
                            perftest::ResultsSet* results);

  static void DisplayMixOutputConfigLegend();
  static void DisplayMixOutputColumnHeader();
  template <fuchsia::media::AudioSampleFormat SampleFormat>
  static void ProfileMixOutput(const OutputProducerConfig& cfg, const Limits& limits,
                               perftest::ResultsSet* results);
};

}  // namespace media::audio::tools

#endif  // SRC_MEDIA_AUDIO_AUDIO_CORE_MIXER_TOOLS_AUDIO_PERFORMANCE_H_
