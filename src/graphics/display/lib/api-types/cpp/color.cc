// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/lib/api-types/cpp/color.h"

#include <fidl/fuchsia.hardware.display.engine/cpp/wire.h>
#include <fuchsia/hardware/display/controller/c/banjo.h>

namespace display {

// static
void Color::StaticAsserts() {
  static_assert(kBytesElements == sizeof(color_t::bytes),
                "Banjo color_t bytes size doesn't match Color");
  static_assert(
      kBytesElements == decltype(fuchsia_hardware_display_types::wire::Color::bytes)::size(),
      "FIDL fuchsia.hardware.display.types/Color bytes size doesn't match Color");
}

}  // namespace display
