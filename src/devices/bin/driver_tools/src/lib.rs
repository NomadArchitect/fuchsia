// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;
mod common;
mod subcommands;

use anyhow::{Context, Result};
use args::{DriverCommand, DriverSubCommand};
use driver_connector::DriverConnector;
use std::io;

#[cfg(not(target_os = "fuchsia"))]
use {futures::lock::Mutex, std::sync::Arc};

pub async fn driver(
    cmd: DriverCommand,
    driver_connector: impl DriverConnector,
    writer: &mut dyn io::Write,
) -> Result<()> {
    match cmd.subcommand {
        #[cfg(not(target_os = "fuchsia"))]
        DriverSubCommand::Conformance(_subcmd) => {
            conformance_lib::conformance(_subcmd, &driver_connector)
                .await
                .context("Conformance subcommand failed")?;
        }
        DriverSubCommand::Device(subcmd) => {
            let dev = driver_connector
                .get_dev_proxy(subcmd.select)
                .await
                .context("Failed to get dev proxy")?;
            subcommands::device::device(subcmd, dev).await.context("Device subcommand failed")?;
        }
        DriverSubCommand::Dump(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::dump::dump(subcmd, writer, driver_development_proxy)
                .await
                .context("Dump subcommand failed")?;
        }
        #[cfg(not(target_os = "fuchsia"))]
        DriverSubCommand::I2c(ref subcmd) => {
            let dev =
                driver_connector.get_dev_proxy(false).await.context("Failed to get dev proxy")?;
            subcommands::i2c::i2c(subcmd, writer, &dev).await.context("I2C subcommand failed")?;
        }
        DriverSubCommand::List(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list::list(subcmd, writer, driver_development_proxy)
                .await
                .context("List subcommand failed")?;
        }
        DriverSubCommand::ListComposites(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list_composites::list_composites(subcmd, writer, driver_development_proxy)
                .await
                .context("List composites subcommand failed")?;
        }
        DriverSubCommand::ListDevices(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list_devices::list_devices(subcmd, driver_development_proxy)
                .await
                .context("List-devices subcommand failed")?;
        }
        DriverSubCommand::ListHosts(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list_hosts::list_hosts(subcmd, driver_development_proxy)
                .await
                .context("List-hosts subcommand failed")?;
        }
        DriverSubCommand::ListCompositeNodeSpecs(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list_composite_node_specs::list_composite_node_specs(
                subcmd,
                writer,
                driver_development_proxy,
            )
            .await
            .context("list-composite-node-specs subcommand failed")?;
        }
        #[cfg(not(target_os = "fuchsia"))]
        DriverSubCommand::Lspci(subcmd) => {
            let dev = driver_connector
                .get_dev_proxy(subcmd.select)
                .await
                .context("Failed to get dev proxy")?;
            subcommands::lspci::lspci(subcmd, &dev).await.context("Lspci subcommand failed")?;
        }
        #[cfg(not(target_os = "fuchsia"))]
        DriverSubCommand::Lsusb(subcmd) => {
            let dev = driver_connector
                .get_dev_proxy(false)
                .await
                .context("Failed to get device watcher proxy")?;
            subcommands::lsusb::lsusb(subcmd, &dev).await.context("Lsusb subcommand failed")?;
        }
        #[cfg(not(target_os = "fuchsia"))]
        DriverSubCommand::PrintInputReport(ref subcmd) => {
            let writer = Arc::new(Mutex::new(io::stdout()));
            let dev =
                driver_connector.get_dev_proxy(false).await.context("Failed to get dev proxy")?;
            subcommands::print_input_report::print_input_report(subcmd, writer, dev)
                .await
                .context("Print-input-report subcommand failed")?;
        }
        DriverSubCommand::Register(subcmd) => {
            let driver_registrar_proxy = driver_connector
                .get_driver_registrar_proxy(subcmd.select)
                .await
                .context("Failed to get driver registrar proxy")?;
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::register::register(
                subcmd,
                writer,
                driver_registrar_proxy,
                driver_development_proxy,
            )
            .await
            .context("Register subcommand failed")?;
        }
        DriverSubCommand::Restart(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::restart::restart(subcmd, writer, driver_development_proxy)
                .await
                .context("Restart subcommand failed")?;
        }
        #[cfg(not(target_os = "fuchsia"))]
        DriverSubCommand::RunTool(subcmd) => {
            let tool_runner_proxy = driver_connector
                .get_tool_runner_proxy(false)
                .await
                .context("Failed to get tool runner proxy")?;
            subcommands::runtool::run_tool(subcmd, writer, tool_runner_proxy)
                .await
                .context("RunTool subcommand failed")?;
        }
        #[cfg(not(target_os = "fuchsia"))]
        DriverSubCommand::StaticChecks(subcmd) => {
            static_checks_lib::static_checks(subcmd, writer)
                .await
                .context("StaticChecks subcommand failed")?;
        }
        DriverSubCommand::TestNode(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::test_node::test_node(&subcmd, driver_development_proxy)
                .await
                .context("AddTestNode subcommand failed")?;
        }
        DriverSubCommand::Disable(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::disable::disable(subcmd, writer, driver_development_proxy)
                .await
                .context("Disable subcommand failed")?;
        }
    };
    Ok(())
}
