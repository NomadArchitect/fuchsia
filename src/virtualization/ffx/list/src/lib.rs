// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::Result, ffx_core::ffx_plugin, ffx_guest_list_args::ListArgs, ffx_writer::Writer,
    fidl_fuchsia_developer_remotecontrol::RemoteControlProxy,
};

#[ffx_plugin("guest_enabled")]
pub async fn guest_list(
    #[ffx(machine = guest_cli::list::GuestList)] writer: Writer,
    args: ListArgs,
    remote_control: RemoteControlProxy,
) -> Result<()> {
    let services = guest_cli::platform::HostPlatformServices::new(remote_control);
    let output = guest_cli::list::handle_list(&services, &args).await?;
    if writer.is_machine() {
        writer.machine(&output)?;
    } else {
        writer.write(format!("{}", output))?;
    }
    Ok(())
}
