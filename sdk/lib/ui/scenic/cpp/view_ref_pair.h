// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_UI_SCENIC_CPP_VIEW_REF_PAIR_H_
#define LIB_UI_SCENIC_CPP_VIEW_REF_PAIR_H_

#include <fidl/fuchsia.ui.views/cpp/fidl.h>
#include <fuchsia/ui/views/cpp/fidl.h>
#include <lib/zx/eventpair.h>

namespace scenic {

struct ViewRefPair {
  // ViewRef creation for the legacy graphics API. For Flatland, use
  // scenic::NewViewIdentityOnCreation().
  static ViewRefPair New();

  fuchsia::ui::views::ViewRefControl control_ref;
  fuchsia::ui::views::ViewRef view_ref;
};

namespace cpp {

struct ViewRefPair {
  // ViewRef creation for the legacy graphics API. For Flatland, use
  // scenic::NewViewIdentityOnCreation().
  static ViewRefPair New();

  fuchsia_ui_views::ViewRefControl control_ref;
  fuchsia_ui_views::ViewRef view_ref;
};

fuchsia_ui_views::ViewRef CloneViewRef(const fuchsia_ui_views::ViewRef& view_ref);

}  // namespace cpp

}  // namespace scenic

#endif  // LIB_UI_SCENIC_CPP_VIEW_REF_PAIR_H_
