// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FFX_LIB_FUCHSIA_CONTROLLER_CPP_FUCHSIA_CONTROLLER_ABI_UTILS_H_
#define SRC_DEVELOPER_FFX_LIB_FUCHSIA_CONTROLLER_CPP_FUCHSIA_CONTROLLER_ABI_UTILS_H_

#include <Python.h>

namespace fuchsia_controller::abi::utils {

// Simple RAII wrapper for PyObject* types.
class Object {
 public:
  explicit Object(PyObject* ptr) : ptr_(ptr) {}
  ~Object() { Py_XDECREF(ptr_); }
  PyObject* get() { return ptr_; }
  PyObject* take() {
    auto res = ptr_;
    ptr_ = nullptr;
    return res;
  }

  // Convenience method for comparing to other pointers.
  bool operator==(PyObject* other) { return other == ptr_; }
  bool operator==(nullptr_t other) { return other == ptr_; }

 private:
  PyObject* ptr_;
};

}  // namespace fuchsia_controller::abi::utils

#endif  // SRC_DEVELOPER_FFX_LIB_FUCHSIA_CONTROLLER_CPP_FUCHSIA_CONTROLLER_ABI_UTILS_H_
