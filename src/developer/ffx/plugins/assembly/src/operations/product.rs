// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::operations::product::assembly_builder::ImageAssemblyConfigBuilder;
use crate::util;
use anyhow::{Context, Result};
use assembly_config_schema::{
    board_config::BoardInformation,
    product_config::{AssemblyConfig, BuildType, FeatureSupportLevel},
};
use assembly_tool::SdkToolProvider;
use camino::Utf8PathBuf;
use ffx_assembly_args::ProductArgs;
use tracing::info;

mod assembly_builder;

pub fn assemble(args: ProductArgs) -> Result<()> {
    let ProductArgs {
        product,
        board_info,
        outdir,
        gendir: _,
        input_bundles_dir,
        legacy_bundle,
        additional_packages_path,
    } = args;

    info!("Loading configuration files.");
    info!("  product: {}", product);

    let config: AssemblyConfig =
        util::read_config(&product).context("Loading product configuration")?;

    let board_info = board_info
        .map(|path| {
            util::read_config::<BoardInformation>(path).context("Loading board information")
        })
        .transpose()?;

    let mut builder = ImageAssemblyConfigBuilder::default();

    // Choose platform bundles based on the chosen base level of support and build type
    //
    // TODO(tbd): Move this to the `assembly_platform_configuration` crate after
    // https://fxrev.dev/694448 lands.
    let mut platform_bundles =
        match (&config.platform.feature_set_level, &config.platform.build_type) {
            (FeatureSupportLevel::Bringup, BuildType::Eng) => {
                vec!["common_bringup", "common_bringup_eng"]
            }
            (FeatureSupportLevel::Minimal, BuildType::Eng) => {
                vec![
                    "common_bringup",
                    "common_bringup_eng",
                    "common_minimal",
                    "common_minimal_eng",
                    "common_minimal_userdebug",
                ]
            }
            (FeatureSupportLevel::Bringup, BuildType::UserDebug) => {
                vec!["common_bringup"]
            }
            (FeatureSupportLevel::Minimal, BuildType::UserDebug) => {
                vec!["common_bringup", "common_minimal", "common_minimal_userdebug"]
            }
            (FeatureSupportLevel::Bringup, BuildType::User) => {
                vec!["common_bringup"]
            }
            (FeatureSupportLevel::Minimal, BuildType::User) => {
                vec!["common_bringup", "common_minimal"]
            }
            _ => vec![],
        };
    platform_bundles.push("emulator_support");

    // Add the platform bundles chosen above.
    for platform_bundle_name in platform_bundles {
        let platform_bundle_path = make_bundle_path(&input_bundles_dir, platform_bundle_name);
        builder.add_bundle(&platform_bundle_path).with_context(|| {
            format!("Adding platform bundle {} ({})", platform_bundle_name, platform_bundle_path)
        })?;
    }

    let legacy_bundle_path = legacy_bundle.join("assembly_config.json");
    builder
        .add_bundle(&legacy_bundle_path)
        .context(format!("Adding legacy bundle: {}", legacy_bundle_path))?;

    // Set structured configuration
    builder.set_bootfs_structured_config(assembly_platform_configuration::define_bootfs_config(
        &config,
        board_info.as_ref(),
    )?);
    for (package, config) in
        assembly_platform_configuration::define_repackaging(&config, board_info.as_ref())?
    {
        builder.set_structured_config(package, config)?;
    }

    // Add product-specified packages and configuration
    builder
        .add_product_packages(config.product.packages)
        .context("Adding product-provided packages")?;

    builder
        .add_product_drivers(config.product.drivers)
        .context("Adding product-provided drivers")?;

    if let Some(package_config_path) = additional_packages_path {
        let additional_packages =
            util::read_config(package_config_path).context("Loading additional package config")?;
        builder.add_product_packages(additional_packages).context("Adding additional packages")?;
    }

    // Get the tool set.
    let tools = SdkToolProvider::try_new()?;

    let image_assembly =
        builder.build(&outdir, &tools).context("Building Image Assembly config")?;
    assembly_validate_product::validate_product(&image_assembly)?;

    let image_assembly_path = outdir.join("image_assembly.json");
    let image_assembly_file = std::fs::File::create(&image_assembly_path)
        .context(format!("Failed to create image assembly config file: {}", image_assembly_path))?;
    serde_json::to_writer_pretty(image_assembly_file, &image_assembly)?;

    Ok(())
}

fn make_bundle_path(bundles_dir: &Utf8PathBuf, name: &str) -> Utf8PathBuf {
    bundles_dir.join(name).join("assembly_config.json")
}
