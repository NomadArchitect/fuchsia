// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_DISPLAY_INFO_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_DISPLAY_INFO_H_

#include <lib/inspect/cpp/inspect.h>
#include <lib/zx/result.h>

#include <cstdint>
#include <memory>
#include <optional>
#include <queue>
#include <string>
#include <string_view>
#include <vector>

#include <fbl/vector.h>

#include "src/graphics/display/drivers/coordinator/added-display-info.h"
#include "src/graphics/display/drivers/coordinator/client-id.h"
#include "src/graphics/display/drivers/coordinator/id-map.h"
#include "src/graphics/display/drivers/coordinator/image.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/display-timing.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/image-id.h"
#include "src/graphics/display/lib/api-types/cpp/pixel-format.h"
#include "src/graphics/display/lib/edid/edid.h"

namespace display_coordinator {

class DisplayInfo : public IdMappable<std::unique_ptr<DisplayInfo>, display::DisplayId> {
 public:
  // Consumes `added_display_info`.
  static zx::result<std::unique_ptr<DisplayInfo>> Create(AddedDisplayInfo added_display_info);

  // Exposed for testing. Prefer obtaining instances from the `Create()` factory method.
  explicit DisplayInfo(display::DisplayId display_id,
                       fbl::Vector<display::PixelFormat> pixel_formats,
                       fbl::Vector<display::DisplayTiming> preferred_modes,
                       std::optional<edid::Edid> edid_info);

  DisplayInfo(const DisplayInfo&) = delete;
  DisplayInfo(DisplayInfo&&) = delete;
  DisplayInfo& operator=(const DisplayInfo&) = delete;
  DisplayInfo& operator=(DisplayInfo&&) = delete;

  ~DisplayInfo();

  // Populates an inspect tree for this display.
  //
  // Must be called after display timing processing is complete. In particular, EDID modes
  // must be parsed and filtered.
  void InitializeInspect(inspect::Node* parent_node);

  // Guaranteed to be >= 0 and < 2^16.
  // Returns zero if the information is not available.
  int GetHorizontalSizeMm() const;

  // Guaranteed to be >= 0 and < 2^16.
  // Returns zero if the information is not available.
  int GetVerticalSizeMm() const;

  // Returns an empty view if the information is not available.
  // The returned string view is guaranteed to be of static storage duration.
  std::string_view GetManufacturerName() const;

  // Returns an empty string if the information is not available.
  std::string GetMonitorName() const;

  // Returns an empty string if the information is not available.
  std::string GetMonitorSerial() const;

  // nullopt if the display does not support EDID.
  const std::optional<edid::Edid> edid_info;

  // Modified after construction if `edid_info` is not nullopt.
  fbl::Vector<display::DisplayTiming> timings;

  const fbl::Vector<display::PixelFormat> pixel_formats;

  // A list of all images which have been sent to display driver.
  Image::DoublyLinkedList images;

  // The number of layers in the applied configuration.
  uint32_t layer_count = 0;

  // Set when a layer change occurs on this display and cleared in vsync
  // when the new layers are all active.
  bool pending_layer_change = false;

  // If a configuration applied by Controller has layer change to occur on the
  // display (i.e. |pending_layer_change| is true), this stores the Controller's
  // config stamp for that configuration; otherwise it stores an invalid stamp.
  display::DriverConfigStamp pending_layer_change_driver_config_stamp;

  // True when we're in the process of switching between display clients.
  bool switching_client = false;

  // |config_image_queue| stores image IDs for each display configurations
  // applied in chronological order.
  // This is used by OnVsync() display events where clients receive image
  // IDs of the latest applied configuration on each Vsync.
  //
  // A |ClientConfigImages| entry is added to the queue once the config is
  // applied, and will be evicted when the config (or a newer config) is
  // already presented on the display at Vsync time.
  //
  // TODO(https://fxbug.dev/42152065): Remove once we remove image IDs in OnVsync() events.
  struct ConfigImages {
    const display::DriverConfigStamp config_stamp;

    struct ImageMetadata {
      display::ImageId image_id;
      ClientId client_id;
    };
    std::vector<ImageMetadata> images;
  };

  std::queue<ConfigImages> config_image_queue;

 private:
  inspect::Node node;
  inspect::ValueList properties;
};

}  // namespace display_coordinator

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_DISPLAY_INFO_H_
