// Copyright 2018 The Chromium Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_CODEC_CODECS_VAAPI_THIRD_PARTY_CHROMIUM_VAAPI_VIDEO_ENCODER_DELEGATE_H_
#define SRC_MEDIA_CODEC_CODECS_VAAPI_THIRD_PARTY_CHROMIUM_VAAPI_VIDEO_ENCODER_DELEGATE_H_

#include <vector>

#include <va/va.h>

// Fuchsia change.
// #include "base/callback.h"
// #include "base/containers/queue.h"
// #include "base/memory/scoped_refptr.h"
// #include "base/sequence_checker.h"
// #include "base/time/time.h"
#include "src/media/third_party/chromium_media/media/base/video_bitrate_allocation.h"
// #include "media/base/video_codecs.h"
#include "src/media/third_party/chromium_media/media/video/video_encode_accelerator.h"
// #include "media/video/video_encoder_info.h"
#include "src/media/codec/codecs/vaapi/vaapi_utils.h"
#include "src/media/third_party/chromium_media/chromium_utils.h"
#include "src/media/third_party/chromium_media/geometry.h"

namespace media {
struct BitstreamBufferMetadata;
class CodecPicture;

// Fuchsia addition:
class VideoFrame {
 public:
  base::TimeDelta timestamp() { return base::TimeDelta(); }
  gfx::Size display_size;
  gfx::Size coded_size;
  size_t stride{};
  uint8_t* base{};
  size_t size_bytes{};
};

class VaapiWrapper;

// An VaapiVideoEncoderDelegate  performs high-level, platform-independent
// encoding process tasks, such as managing codec state, reference frames, etc.,
// but may require support from an external accelerator (typically a hardware
// accelerator) to offload some stages of the actual encoding process, using
// the parameters that the Delegate prepares beforehand.
//
// For each frame to be encoded, clients provide an EncodeJob object to be set
// up by a Delegate subclass with job parameters, and execute the job
// afterwards. Any resources required for the job are also provided by the
// clients, and associated with the EncodeJob object.
class VaapiVideoEncoderDelegate {
 public:
  VaapiVideoEncoderDelegate(scoped_refptr<VaapiWrapper> vaapi_wrapper,
                            base::RepeatingClosure error_cb);
  virtual ~VaapiVideoEncoderDelegate();

  enum class BitrateControl {
    kConstantBitrate,               // Constant Bitrate mode. This class relies on other
                                    // parts (e.g. driver) to achieve the specified bitrate.
    kConstantQuantizationParameter  // Constant Quantization Parameter mode.
                                    // This class needs to compute a proper
                                    // quantization parameter and give other
                                    // parts (e.g. the driver) the value.
  };

  struct Config {
    // Maxium number of reference frames.
    // For H.264 encoding, the value represents the maximum number of reference
    // frames for both the reference picture list 0 (bottom 16 bits) and the
    // reference picture list 1 (top 16 bits).
    size_t max_num_ref_frames;

    bool native_input_mode = false;

    BitrateControl bitrate_control = BitrateControl::kConstantBitrate;
  };

  // EncodeResult owns the necessary resource to keep the encoded buffer. The
  // encoded buffer can be downloaded with the EncodeResult, for example, by
  // calling VaapiWrapper::DownloadFromVABuffer().
  class EncodeResult {
   public:
    EncodeResult(std::unique_ptr<ScopedVABuffer> coded_buffer,
                 const BitstreamBufferMetadata& metadata);
    ~EncodeResult();

    VABufferID coded_buffer_id() const;
    const BitstreamBufferMetadata& metadata() const;

   private:
    const std::unique_ptr<ScopedVABuffer> coded_buffer_;
    const BitstreamBufferMetadata metadata_;
  };

  // An abstraction of an encode job for one frame. Parameters required for an
  // EncodeJob to be executed are prepared by an VaapiVideoEncoderDelegate,
  // while the accelerator-specific callbacks required to set up and execute it
  // are provided by the accelerator itself, based on these parameters.
  // Accelerators are also responsible for providing any resources (such as
  // memory for output, etc.) as needed.
  class EncodeJob {
   public:
    // Creates an EncodeJob to encode |input_frame|, which will be executed by
    // calling ExecuteSetupCallbacks() in VaapiVideoEncoderDelegate::Encode().
    // If |keyframe| is true, requests this job to produce a keyframe.
    EncodeJob(scoped_refptr<VideoFrame> input_frame, bool keyframe);
    // Constructor for VA-API.
    EncodeJob(scoped_refptr<VideoFrame> input_frame, bool keyframe, VASurfaceID input_surface_id,
              const gfx::Size& input_surface_size, scoped_refptr<CodecPicture> picture,
              std::unique_ptr<ScopedVABuffer> coded_buffer);

    EncodeJob(const EncodeJob&) = delete;
    EncodeJob& operator=(const EncodeJob&) = delete;

    ~EncodeJob();

    // Creates EncodeResult with |metadata|. This passes ownership of the
    // resources owned by EncodeJob and therefore must be called with
    // std::move().
    std::unique_ptr<EncodeResult> CreateEncodeResult(const BitstreamBufferMetadata& metadata) &&;

    // Requests this job to produce a keyframe; requesting a keyframe may not
    // always result in one being produced by the encoder (e.g. if it would
    // not fit in the bitrate budget).
    void ProduceKeyframe() { keyframe_ = true; }

    // Returns true if this job has been requested to produce a keyframe.
    bool IsKeyframeRequested() const { return keyframe_; }

    base::TimeDelta timestamp() const;

    const scoped_refptr<VideoFrame>& input_frame() const;

    // VA-API specific methods.
    VABufferID coded_buffer_id() const;
    VASurfaceID input_surface_id() const;
    const gfx::Size& input_surface_size() const;
    const scoped_refptr<CodecPicture>& picture() const;

   private:
    // Input VideoFrame to be encoded.
    const scoped_refptr<VideoFrame> input_frame_;

    // True if this job is to produce a keyframe.
    bool keyframe_;

    // VA-API specific members.
    // Input surface ID and size for video frame data or scaled data.
    const VASurfaceID input_surface_id_;
    const gfx::Size input_surface_size_;
    const scoped_refptr<CodecPicture> picture_;
    // Buffer that will contain the output bitstream data for this frame.
    std::unique_ptr<ScopedVABuffer> coded_buffer_;
  };

  // Initializes the encoder with requested parameter set |config| and
  // |ave_config|. Returns false if the requested set of parameters is not
  // supported, true on success.
  virtual bool Initialize(const VideoEncodeAccelerator::Config& config,
                          const VaapiVideoEncoderDelegate::Config& ave_config) = 0;

  // Updates current framerate and/or bitrate to |framerate| in FPS
  // and the specified video bitrate allocation.
  virtual bool UpdateRates(const VideoBitrateAllocation& bitrate_allocation,
                           uint32_t framerate) = 0;

  // Returns coded size for the input buffers required to encode, in pixels;
  // typically visible size adjusted to match codec alignment requirements.
  virtual gfx::Size GetCodedSize() const = 0;

  // Returns minimum size in bytes for bitstream buffers required to fit output
  // stream buffers produced.
  virtual size_t GetBitstreamBufferSize() const;

  // Returns maximum number of reference frames that may be used by the
  // encoder to encode one frame. The client should be able to provide up to
  // at least this many frames simultaneously for encode to make progress.
  virtual size_t GetMaxNumOfRefFrames() const = 0;

  // Prepares and submits the encode operation to underlying driver for an
  // EncodeJob for one frame and returns true on success.
  bool Encode(EncodeJob& encode_job);

  // Creates and returns the encode result for specified EncodeJob by
  // synchronizing the corresponding encode operation.
  std::unique_ptr<EncodeResult> GetEncodeResult(std::unique_ptr<EncodeJob> encode_job);

  // Gets the active spatial layer resolutions for K-SVC encoding, VaapiVEA
  // can get this info from the encoder delegate. Returns empty vector on
  // failure.
  virtual std::vector<gfx::Size> GetSVCLayerResolutions() = 0;

 protected:
  virtual BitstreamBufferMetadata GetMetadata(const EncodeJob& encode_job, size_t payload_size);

  const scoped_refptr<VaapiWrapper> vaapi_wrapper_;

  base::RepeatingClosure error_cb_;

  bool native_input_mode_ = false;

  SEQUENCE_CHECKER(sequence_checker_);

 private:
  // Prepares a new |encode_job| to be executed in Accelerator and returns true
  // on success. The caller may then call ExecuteSetupCallbacks() on the job to
  // run them.
  virtual bool PrepareEncodeJob(EncodeJob& encode_job) = 0;

  // Notifies the encoded chunk size in bytes to update a bitrate controller in
  // VaapiVideoEncoderDelegate. This should be called only if
  // VaapiVideoEncoderDelegate is configured with
  // BitrateControl::kConstantQuantizationParameter.
  virtual void BitrateControlUpdate(uint64_t encoded_chunk_size_bytes);
};
}  // namespace media

#endif  // SRC_MEDIA_CODEC_CODECS_VAAPI_THIRD_PARTY_CHROMIUM_VAAPI_VIDEO_ENCODER_DELEGATE_H_
