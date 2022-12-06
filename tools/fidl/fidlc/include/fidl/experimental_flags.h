// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef TOOLS_FIDL_FIDLC_INCLUDE_FIDL_EXPERIMENTAL_FLAGS_H_
#define TOOLS_FIDL_FIDLC_INCLUDE_FIDL_EXPERIMENTAL_FLAGS_H_

#include <lib/fit/function.h>

#include <map>
#include <string_view>

namespace fidl {

class ExperimentalFlags {
 public:
  using FlagSet = uint32_t;
  enum class Flag : FlagSet {
    // Used for testing, and to keep an "example experiment" if we ever have no experiments at
    // all.
    kNoop = 1 << 0,

    kAllowNewTypes = 1 << 1,
    // TODO(fxbug.dev/88366): Remove once unknown interactions are supported.
    kUnknownInteractions = 1 << 2,
    kNoOptionalStructs = 1 << 3,
    kOutputIndexJson = 1 << 4,

    // TODO(fxbug.dev/110021): A temporary measure describe in
    // fxbug.dev/110294.
    kZxCTypes = 1 << 5,

    // TODO(fxbug.dev/112767): Remove once soft transition is done.
    kSimpleEmptyResponseSyntax = 1 << 6,

    // TODO(fxbug.dev/88366): Remove once unknown interactions are supported.
    kUnknownInteractionsMigration = 1 << 7,

    // TODO(fxbug.dev/100478): Allows backends to implement overflowing experiments.
    kAllowOverflowing = 1 << 8,
  };

  ExperimentalFlags() = default;
  explicit ExperimentalFlags(Flag flag) : flags_(static_cast<FlagSet>(flag)) {}

  bool EnableFlagByName(std::string_view flag);
  void EnableFlag(Flag flag);

  bool IsFlagEnabled(Flag flag) const;
  void ForEach(const fit::function<void(const std::string_view, Flag, bool)>& fn) const;

 private:
  static std::map<const std::string_view, const Flag> FLAG_STRINGS;

  FlagSet flags_{0};
};

}  // namespace fidl

#endif  // TOOLS_FIDL_FIDLC_INCLUDE_FIDL_EXPERIMENTAL_FLAGS_H_
