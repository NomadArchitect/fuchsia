// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[repr(C)]
#[derive(
    IntoBytes, KnownLayout, FromBytes, Immutable, PartialEq, Eq, Clone, Copy, Debug, Default,
)]
pub struct StatusCode(pub u16);

impl StatusCode {
    pub fn into_fidl_or_refused_unspecified(self) -> fidl_ieee80211::StatusCode {
        self.try_into().unwrap_or(fidl_ieee80211::StatusCode::RefusedReasonUnspecified)
    }
}

impl From<fidl_ieee80211::StatusCode> for StatusCode {
    fn from(fidl_status_code: fidl_ieee80211::StatusCode) -> StatusCode {
        StatusCode(fidl_status_code as u16)
    }
}

// TODO(https://fxbug.dev/42080459): Replace uses of this `From` implementation with `TryFrom` and then
//                         remove this.
impl From<StatusCode> for Option<fidl_ieee80211::StatusCode> {
    fn from(status_code: StatusCode) -> Option<fidl_ieee80211::StatusCode> {
        fidl_ieee80211::StatusCode::from_primitive(status_code.0)
    }
}

impl TryFrom<StatusCode> for fidl_ieee80211::StatusCode {
    type Error = ();

    fn try_from(status: StatusCode) -> Result<fidl_ieee80211::StatusCode, ()> {
        fidl_ieee80211::StatusCode::from_primitive(status.0).ok_or(())
    }
}
