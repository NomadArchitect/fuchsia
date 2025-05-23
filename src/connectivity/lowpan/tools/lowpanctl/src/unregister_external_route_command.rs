// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::context::LowpanCtlContext;
use crate::prelude::*;
use fidl_fuchsia_net::{Ipv6Address, Ipv6AddressWithPrefix as Ipv6Subnet};

/// Contains the arguments decoded for the `unregister-external-route` command.
#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "unregister-external-route")]
pub struct UnregisterExternalRouteCommand {
    /// ipv6 prefix (always a /64)
    #[argh(positional)]
    pub addr: std::net::Ipv6Addr,
}

impl UnregisterExternalRouteCommand {
    pub async fn exec(&self, context: &mut LowpanCtlContext) -> Result<(), Error> {
        let device_route = context.get_default_device_route_proxy().await?;
        let prefix_len = 64;
        let subnet = Ipv6Subnet { addr: Ipv6Address { addr: self.addr.octets() }, prefix_len };

        device_route
            .unregister_external_route(&subnet)
            .await
            .context("Unable to send unregister_external_route command")
    }
}
