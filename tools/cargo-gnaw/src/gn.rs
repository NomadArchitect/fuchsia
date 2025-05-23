// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::cfg::{cfg_to_gn_conditional, target_to_gn_conditional};
use crate::target::GnTarget;
use crate::types::*;
use crate::{
    BinaryRenderOptions, CombinedTargetCfg, GlobalTargetCfgs, GroupVisibility, RuleRenaming,
};
use anyhow::{Context, Result};
use cargo_metadata::Package;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Display;
use std::io;
use std::path::Path;
use walkdir::WalkDir;

/// Utility to add a version suffix to a GN target name.
pub fn add_version_suffix(prefix: &str, version: &impl ToString) -> String {
    let mut accum = String::new();
    accum.push_str(&prefix);
    accum.push_str("-v");
    accum.push_str(version.to_string().replace('.', "_").as_str());
    accum
}

/// Write a header for the output GN file
pub fn write_header<W: io::Write>(output: &mut W, _cargo_file: &Path) -> Result<()> {
    writeln!(
        output,
        include_str!("../templates/gn_header.template"),
        // TODO set this, but in a way that tests don't fail on Jan 1st
        year = "2020",
    )
    .map_err(Into::into)
}

pub fn write_fuchsia_sdk_metadata_header<W: io::Write>(output: &mut W) -> Result<()> {
    writeln!(output, include_str!("../templates/gn_sdk_metadata_header.template"),)
        .map_err(Into::into)
}

/// Write an import stament for the output GN file
pub fn write_import<W: io::Write>(output: &mut W, file_name: &str) -> Result<()> {
    writeln!(output, include_str!("../templates/gn_import.template"), file_name = file_name)
        .map_err(Into::into)
}

/// Writes rules at the top of the GN file that don't have the version appended
///
/// Args:
///   - `rule_renaming`: if set, the renaming rules will be used to generate
///     appropriate group and rule names. See [RuleRenaming] for details.
pub fn write_top_level_rule<W: io::Write>(
    output: &mut W,
    platform: Option<&str>,
    pkg: &Package,
    group_visibility: Option<&GroupVisibility>,
    rule_renaming: Option<&RuleRenaming>,
    testonly: bool,
    has_tests: bool,
) -> Result<()> {
    let target_name = if pkg.is_proc_macro() {
        format!("{}($host_toolchain)", pkg.gn_name())
    } else {
        pkg.gn_name()
    };
    if let Some(ref platform) = platform {
        writeln!(
            output,
            "if ({conditional}) {{\n",
            conditional = target_to_gn_conditional(platform)?
        )?;
    }

    let group_rule = rule_renaming.and_then(|t| t.group_name.as_deref()).unwrap_or("group");
    let optional_visibility =
        group_visibility.map(|v| format!("visibility = {}", v.variable)).unwrap_or_default();
    writeln!(
        output,
        include_str!("../templates/top_level_gn_rule.template"),
        group_name = pkg.name,
        dep_name = target_name,
        group_rule_name = group_rule,
        optional_visibility = optional_visibility,
        optional_testonly = if testonly { "testonly = true" } else { "" },
    )?;

    if has_tests {
        let name = pkg.name.clone() + "-test";
        writeln!(
            output,
            include_str!("../templates/top_level_gn_rule.template"),
            group_name = name,
            group_rule_name = group_rule,
            dep_name = target_name + "-test",
            optional_visibility = optional_visibility,
            optional_testonly = "testonly = true",
        )?;
    }

    if platform.is_some() {
        writeln!(output, "}}\n")?;
    }
    Ok(())
}

/// Writes Fuchsia SDK metadata for a top-level rule.
pub fn write_fuchsia_sdk_metadata<W: io::Write>(
    output: &mut W,
    platform: Option<&str>,
    pkg: &Package,
) -> Result<()> {
    // TODO: add features and registry to metadata
    let platform_constraint = if let Some(p) = platform {
        format!(" && {}", target_to_gn_conditional(p)?)
    } else {
        "".to_owned()
    };

    writeln!(
        output,
        r#" if (_generating_sdk{constraint}) {{
            sdk_atom("{group_name}_sdk") {{
                id = "sdk://${{_sdk_prefix}}third_party/rust_crates/{group_name}"
                category = "partner"
                meta = {{
                    value = {{
                        type = "{sdk_atom_type}"
                        name = "{group_name}"
                        version = "{version}"
                    }}
                    dest = "${{_sdk_prefix}}third_party/rust_crates/{group_name}/meta.json"
                    type = "{sdk_atom_type}"
                }}
            }}
        }}
        "#,
        constraint = platform_constraint,
        group_name = pkg.name,
        sdk_atom_type = "rust_3p_library",
        version = pkg.version,
    )?;

    Ok(())
}

/// Writes rules at the top of the GN file that don't have the version appended
pub fn write_binary_top_level_rule<'a, W: io::Write>(
    output: &mut W,
    platform: Option<String>,
    target: &GnTarget<'a>,
    options: &BinaryRenderOptions<'_>,
) -> Result<()> {
    if let Some(ref platform) = platform {
        writeln!(
            output,
            "if ({conditional}) {{\n",
            conditional = cfg_to_gn_conditional(platform)?
        )?;
    }
    writeln!(
        output,
        include_str!("../templates/top_level_binary_gn_rule.template"),
        group_name = options.binary_name,
        dep_name = target.gn_target_name(),
        optional_testonly = "",
    )?;

    if options.tests_enabled {
        let name = options.binary_name.to_owned() + "-test";
        let dep_name = target.gn_target_name() + "-test";
        writeln!(
            output,
            include_str!("../templates/top_level_binary_gn_rule.template"),
            group_name = name,
            dep_name = dep_name,
            optional_testonly = "testonly = true",
        )?;
    }

    if platform.is_some() {
        writeln!(output, "}}\n")?;
    }
    Ok(())
}

struct GnField {
    ty: String,
    exists: bool,
    // Use BTreeMap so that iteration over platforms is stable.
    add_fields: BTreeMap<Option<Platform>, Vec<String>>,
    remove_fields: BTreeMap<Option<Platform>, Vec<String>>,
}
impl GnField {
    /// If defining a new field in the template
    pub fn new(ty: &str) -> GnField {
        GnField {
            ty: ty.to_string(),
            exists: false,
            add_fields: BTreeMap::new(),
            remove_fields: BTreeMap::new(),
        }
    }

    /// If the field already exists in the template
    pub fn exists(ty: &str) -> GnField {
        GnField { exists: true, ..Self::new(ty) }
    }

    pub fn add_platform_cfg<T: AsRef<str> + Display>(&mut self, platform: Option<String>, cfg: T) {
        let field = self.add_fields.entry(platform).or_default();
        field.push(format!("\"{}\"", cfg));
    }

    pub fn remove_platform_cfg<T: AsRef<str> + Display>(
        &mut self,
        platform: Option<String>,
        cfg: T,
    ) {
        let field = self.remove_fields.entry(platform).or_default();
        field.push(format!("\"{}\"", cfg));
    }

    pub fn add_cfg<T: AsRef<str> + Display>(&mut self, cfg: T) {
        self.add_platform_cfg(None, cfg)
    }

    pub fn remove_cfg<T: AsRef<str> + Display>(&mut self, cfg: T) {
        self.remove_platform_cfg(None, cfg)
    }

    pub fn render_gn(&self) -> String {
        let mut output = if self.exists {
            // We don't create an empty [] if the field already exists
            match self.add_fields.get(&None) {
                Some(add_fields) => format!("{} += [{}]\n", self.ty, add_fields.join(",")),
                None => "".to_string(),
            }
        } else {
            format!("{} = [{}]\n", self.ty, self.add_fields.get(&None).unwrap_or(&vec![]).join(","))
        };

        // remove platfrom independent configs
        if let Some(rm_fields) = self.remove_fields.get(&None) {
            output.push_str(format!("{} -= [{}]\n", self.ty, rm_fields.join(",")).as_str());
        }

        // Add logic for specific platforms
        for platform in self.add_fields.keys().filter(|k| k.is_some()) {
            output.push_str(
                format!(
                    "if ({}) {{\n",
                    cfg_to_gn_conditional(platform.as_ref().unwrap()).expect("valid cfg")
                )
                .as_str(),
            );
            output.push_str(
                format!(
                    "{} += [{}]",
                    self.ty,
                    self.add_fields.get(platform).unwrap_or(&vec![]).join(",")
                )
                .as_str(),
            );
            output.push_str("}\n");
        }

        // Remove logic for specific platforms
        for platform in self.remove_fields.keys().filter(|k| k.is_some()) {
            output.push_str(
                format!(
                    "if ({}) {{\n",
                    cfg_to_gn_conditional(platform.as_ref().unwrap()).expect("valid cfg")
                )
                .as_str(),
            );
            output.push_str(
                format!(
                    "{} -= [{}]",
                    self.ty,
                    self.remove_fields.get(platform).unwrap_or(&vec![]).join(",")
                )
                .as_str(),
            );
            output.push_str("}\n");
        }
        output
    }
}

/// Write a Target to the GN file.
///
/// Includes information from the build script.
///
/// Args:
///   - `renamed_rule`: if this is set, the rule that is written out is changed
///     to be the content of this arg.
pub fn write_rule<W: io::Write>(
    output: &mut W,
    target: &GnTarget<'_>,
    project_root: &Path,
    global_target_cfgs: Option<&GlobalTargetCfgs>,
    custom_build: Option<&CombinedTargetCfg<'_>>,
    output_name: Option<&str>,
    is_testonly: bool,
    is_test: bool,
    renamed_rule: Option<&str>,
    scan_for_licenses: bool,
) -> Result<()> {
    // Generate a section for dependencies that is paramaterized on toolchain
    let mut dependencies = String::from("deps = []\n");
    let mut aliased_deps = vec![];

    // Stable output of platforms
    let mut platform_deps: Vec<(
        &Option<String>,
        &Vec<(&cargo_metadata::Package, std::string::String)>,
    )> = target.dependencies.iter().collect();
    platform_deps.sort_by(|p, p2| p.0.cmp(p2.0));

    for (platform, deps) in platform_deps {
        // sort for stable output
        let mut deps = deps.clone();
        deps.sort_by(|a, b| (a.0).id.cmp(&(b.0).id));

        // TODO(bwb) feed GN toolchain mapping in as a configuration to make more generic
        match platform.as_ref().map(String::as_str) {
            None => {
                for pkg in deps {
                    dependencies.push_str("  deps += [");
                    if pkg.0.is_proc_macro() {
                        dependencies.push_str(
                            format!("\":{}($host_toolchain)\"", pkg.0.gn_name()).as_str(),
                        );
                    } else {
                        dependencies.push_str(format!("\":{}\"", pkg.0.gn_name()).as_str());
                    }
                    dependencies.push_str("]\n");
                    if pkg.0.name.replace("-", "_") != pkg.1 {
                        aliased_deps.push(format!("{} = \":{}\" ", pkg.1, pkg.0.gn_name()));
                    }
                }
            }
            Some(platform) => {
                dependencies.push_str(
                    format!("if ({}) {{\n", target_to_gn_conditional(platform)?).as_str(),
                );
                for pkg in deps {
                    dependencies.push_str("  deps += [");
                    if pkg.0.is_proc_macro() {
                        dependencies.push_str(
                            format!("\":{}($host_toolchain)\"", pkg.0.gn_name()).as_str(),
                        );
                    } else {
                        dependencies.push_str(format!("\":{}\"", pkg.0.gn_name()).as_str());
                    }
                    dependencies.push_str("]\n");

                    if pkg.0.name.replace("-", "_") != pkg.1 {
                        aliased_deps.push(format!("{} = \":{}\" ", pkg.1, pkg.0.gn_name()));
                    }
                }
                dependencies.push_str("}\n");
            }
        }
    }

    // write the features into the configs
    let mut rustflags = GnField::new("rustflags");
    let mut rustenv = GnField::new("rustenv");
    let mut configs = GnField::exists("configs");
    let mut require_licenses = false;

    if let Some(global_cfg) = global_target_cfgs {
        for cfg in &global_cfg.remove_cfgs {
            configs.remove_cfg(cfg);
        }
        for cfg in &global_cfg.add_cfgs {
            configs.add_cfg(cfg);
        }
        if let Some(ref require) = global_cfg.require_licenses {
            require_licenses = *require;
        }
    }

    // Associate unique metadata with this crate
    rustflags.add_cfg("--cap-lints=allow");
    rustflags.add_cfg(format!("--edition={}", target.edition));
    rustflags.add_cfg(format!("-Cmetadata={}", target.metadata_hash()));
    rustflags.add_cfg(format!("-Cextra-filename=-{}", target.metadata_hash()));
    if is_test {
        rustflags.add_cfg("--test");
    }

    // Aggregate feature flags
    for feature in target.features {
        rustflags.add_cfg(format!("--cfg=feature=\\\"{}\\\"", feature));
    }

    // From the gn custom configs, add flags, env vars, and visibility
    let mut visibility = vec![];

    let mut uses_fuchsia_license = false;
    let mut license_files = vec![];

    if let Some(custom_build) = custom_build {
        for (platform, cfg) in custom_build {
            if let Some(ref deps) = cfg.deps {
                let build_deps = |dependencies: &mut String| {
                    for dep in deps {
                        dependencies.push_str(format!("  deps += [\"{}\"]", dep).as_str());
                    }
                };
                if let Some(platform) = platform {
                    dependencies.push_str(
                        format!("if ({}) {{\n", target_to_gn_conditional(platform)?).as_str(),
                    );
                    build_deps(&mut dependencies);
                    dependencies.push_str("}\n");
                } else {
                    build_deps(&mut dependencies);
                }
            }
            if let Some(ref flags) = cfg.rustflags {
                for flag in flags {
                    rustflags.add_platform_cfg(platform.cloned(), flag.to_string());
                }
            }
            if let Some(ref env_vars) = cfg.env_vars {
                for flag in env_vars {
                    rustenv.add_platform_cfg(platform.cloned(), flag.to_string());
                }
            }
            if let Some(ref crate_configs) = cfg.configs {
                for config in crate_configs {
                    configs.add_platform_cfg(platform.cloned(), config);
                }
            }
            if let Some(ref vis) = cfg.visibility {
                visibility.extend(vis.iter().map(|v| format!("  visibility += [\"{}\"]", v)));
            }
            if let Some(ref uses) = cfg.uses_fuchsia_license {
                uses_fuchsia_license = *uses;
            }
            if let Some(ref lics) = cfg.license_files {
                license_files = (*lics.clone()).to_vec();
            }
        }
    }

    let visibility = if visibility.is_empty() {
        String::from("visibility = [\":*\"]\n")
    } else {
        let mut v = String::from("visibility = []\n");
        v.extend(visibility);
        v
    };

    // making the templates more readable.
    let aliased_deps_str = if aliased_deps.is_empty() {
        String::from("")
    } else {
        format!("aliased_deps = {{{}}}", aliased_deps.join("\n"))
    };

    // GN root relative path
    let root_relative_path = format!(
        "//{}",
        target
            .crate_root
            .canonicalize_utf8()
            .unwrap()
            .strip_prefix(project_root)
            .with_context(|| format!(
                "{} is located outside of the project. Check your vendoring setup",
                target.name()
            ))?
            .to_string()
            // NOTE: This is necessary because '+' is not a allowed in Bazel labels. For this
            // reason, crate_universe replaces '+' in directory names of vendored crates with '-'.
            // See https://github.com/bazelbuild/rules_rust/blob/1c0de8a98c4e10091155627bf5ee926653334504/crate_universe/src/utils.rs#L50
            .replace('+', "-")
    );
    let output_name = if is_test {
        output_name.map_or_else(
            || {
                Cow::Owned(format!(
                    "{}-{}-test",
                    target.name().replace('-', "_"),
                    target.metadata_hash()
                ))
            },
            |n| Cow::Owned(format!("{}-test", n)),
        )
    } else {
        output_name.map_or_else(
            || {
                Cow::Owned(format!(
                    "{}-{}",
                    target.name().replace('-', "_"),
                    target.metadata_hash()
                ))
            },
            Cow::Borrowed,
        )
    };
    let mut target_name = target.gn_target_name();
    if is_test {
        target_name.push_str("-test");
    }

    let optional_testonly = if is_testonly || is_test { "testonly = true" } else { "" };

    let gn_rule = if let Some(renamed_rule) = renamed_rule {
        renamed_rule.to_owned()
    } else if is_test {
        "executable".to_owned()
    } else {
        target.gn_target_type()
    };

    let mut license_files_found = false;
    let mut applicable_licenses = GnField::new("applicable_licenses");
    let gn_crate_name = target.name().replace('-', "_");
    let mut license_file_labels = vec![];

    let mut create_license_target =
        |license_file_labels: &Vec<String>| -> Result<(), std::io::Error> {
            // Define a license target
            let license_target_name = format!("{}.license", target_name);
            // Directory iteration order is random, so sort alphabetically.
            let mut license_files_param = GnField::new("license_files");
            for entry in license_file_labels {
                license_files_param.add_cfg(entry);
            }

            writeln!(
                output,
                include_str!("../templates/gn_license.template"),
                target_name = license_target_name,
                public_package_name = gn_crate_name,
                license_files = license_files_param.render_gn(),
            )?;

            applicable_licenses.add_cfg(format!(":{}", license_target_name));
            Ok(())
        };

    if !license_files.is_empty() {
        for license_file in license_files {
            let license_file_label = format!(
                "//{}/{}",
                target
                    .package_root()
                    .canonicalize()
                    .unwrap()
                    .strip_prefix(project_root)
                    .unwrap()
                    .to_string_lossy(),
                license_file
            );
            license_file_labels.push(license_file_label);
        }
        license_file_labels.sort();
        create_license_target(&license_file_labels)?;
    } else if scan_for_licenses {
        // Scan for LICENSE* files in the crate's root dir.
        // Disabled in unit tests, where package_root always fails.

        fn generate_license_label(entry: &walkdir::DirEntry, project_root: &Path) -> String {
            format!(
                "//{}",
                entry
                    .path()
                    .canonicalize()
                    .unwrap()
                    .strip_prefix(project_root)
                    .unwrap()
                    .to_string_lossy()
            )
        }

        for entry in WalkDir::new(target.package_root()).follow_links(false).into_iter() {
            let entry = entry.unwrap();

            let file_name = entry.file_name().to_string_lossy();
            let file_name_lower = file_name.to_lowercase();
            if file_name_lower.starts_with("license")
                || file_name_lower.starts_with("licence")
                || file_name_lower.starts_with("copyright")
            {
                if entry.file_type().is_file() {
                    let license_file_label = generate_license_label(&entry, project_root);
                    license_file_labels.push(license_file_label);
                    license_files_found = true;
                } else if entry.file_type().is_dir() && file_name == "LICENSES" {
                    // A directory named "LICENSE" was found.
                    // Assume all files within are licenses and collect them.
                    for inner_entry in WalkDir::new(entry.path()).follow_links(false).into_iter() {
                        let inner_entry = inner_entry.unwrap();
                        if inner_entry.file_type().is_file() {
                            let license_file_label =
                                generate_license_label(&inner_entry, project_root);
                            license_file_labels.push(license_file_label);
                            license_files_found = true;
                        }
                    }
                }
            }
        }

        if license_files_found {
            if uses_fuchsia_license {
                anyhow::bail!("ERROR: Crate {}.{} has license files but is set with uses_fuchsia_license = true", target.name(), target.version())
            }
            license_file_labels.sort();
            create_license_target(&license_file_labels)?;
        } else if uses_fuchsia_license ||
            // Empty crates are stubs crates authored by Fuchsia.
            target.package_root().parent().unwrap().ends_with("third_party/rust_crates/empty")
        {
            applicable_licenses.add_cfg("//build/licenses:fuchsia_license");
        } else if require_licenses {
            anyhow::bail!(
                "ERROR: Crate at {} must have LICENSE* or COPYRIGHT files.

Make sure such files are placed at the crate's root folder.

Alternatively, if the crate's license is the same as Fuchsia's,
modify //third_party/rust_crates/Cargo.toml by adding:

```
[gn.package.{}.\"{}\"]
uses_fuchsia_license = true
```",
                target.package_root(),
                target.name(),
                target.version()
            );
        }
    }

    writeln!(
        output,
        include_str!("../templates/gn_rule.template"),
        gn_rule = gn_rule,
        target_name = target_name,
        crate_name = gn_crate_name,
        output_name = output_name,
        root_path = root_relative_path,
        aliased_deps = aliased_deps_str,
        dependencies = dependencies,
        cfgs = configs.render_gn(),
        rustenv = rustenv.render_gn(),
        rustflags = rustflags.render_gn(),
        visibility = visibility,
        optional_testonly = optional_testonly,
        applicable_licenses = applicable_licenses.render_gn(),
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use cargo_metadata::Edition;
    use semver::Version;
    use std::collections::HashMap;

    // WARNING: the expected output tests below have non-printable artifacts
    // due to the way the templates are generated. Do not remove extra spaces
    // in the quoted strings.

    #[test]
    fn canonicalized_paths() {
        let pkg_id = cargo_metadata::PackageId { repr: String::from("42") };
        let version = Version::new(0, 1, 0);

        let mut project_root = std::env::temp_dir();
        project_root.push(Path::new("canonicalized_paths"));
        std::fs::write(&project_root, "").expect("write to temp file");

        let target = GnTarget::new(
            &pkg_id,
            "test_target",
            "test_package",
            Edition::E2018,
            Utf8Path::from_path(project_root.as_path()).unwrap(),
            &version,
            GnRustType::Library,
            &[],
            false,
            HashMap::new(),
        );

        let not_prefix = Path::new("/foo");
        let prefix = std::env::temp_dir();

        let mut output = vec![];
        assert!(write_rule(
            &mut output,
            &target,
            not_prefix,
            None,
            None,
            None,
            false,
            false,
            None,
            false
        )
        .is_err());
        assert!(write_rule(
            &mut output,
            &target,
            prefix.as_path(),
            None,
            None,
            None,
            false,
            false,
            None,
            false,
        )
        .is_ok());
    }

    #[test]
    fn simple_target() {
        let pkg_id = cargo_metadata::PackageId { repr: String::from("42") };
        let version = Version::new(0, 1, 0);

        let mut project_root = std::env::temp_dir();
        project_root.push(Path::new("simple_target"));
        std::fs::write(&project_root, "").expect("write to temp file");

        let target = GnTarget::new(
            &pkg_id,
            "test_target",
            "test_package",
            Edition::E2018,
            Utf8Path::from_path(project_root.as_path()).unwrap(),
            &version,
            GnRustType::Library,
            &[],
            false,
            HashMap::new(),
        );

        let mut output = vec![];
        write_rule(
            &mut output,
            &target,
            std::env::temp_dir().as_path(),
            None,
            None,
            None,
            false,
            false,
            None,
            false,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output,
            r#"rust_library("test_package-v0_1_0") {
  crate_name = "test_target"
  crate_root = "//simple_target"
  output_name = "test_target-c5bf97c44457465a"
  
  deps = []

  rustenv = []

  rustflags = ["--cap-lints=allow","--edition=2018","-Cmetadata=c5bf97c44457465a","-Cextra-filename=-c5bf97c44457465a"]

  
  visibility = [":*"]

  
  applicable_licenses = []

}

"#
        );
    }

    #[test]
    fn binary_target() {
        let pkg_id = cargo_metadata::PackageId { repr: String::from("42") };
        let version = Version::new(0, 1, 0);

        let mut project_root = std::env::temp_dir();
        project_root.push(Path::new("binary_target"));
        std::fs::write(&project_root, "").expect("write to temp file");

        let target = GnTarget::new(
            &pkg_id,
            "test_target",
            "test_package",
            Edition::E2018,
            Utf8Path::from_path(project_root.as_path()).unwrap(),
            &version,
            GnRustType::Binary,
            &[],
            false,
            HashMap::new(),
        );

        let outname = Some("rainbow_binary");
        let mut output = vec![];
        write_rule(
            &mut output,
            &target,
            std::env::temp_dir().as_path(),
            None,
            None,
            outname,
            false,
            false,
            None,
            false,
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output,
            r#"executable("test_package-test_target-v0_1_0") {
  crate_name = "test_target"
  crate_root = "//binary_target"
  output_name = "rainbow_binary"
  
  deps = []

  rustenv = []

  rustflags = ["--cap-lints=allow","--edition=2018","-Cmetadata=bf8f4a806276c599","-Cextra-filename=-bf8f4a806276c599"]

  
  visibility = [":*"]

  
  applicable_licenses = []

}

"#
        );
    }

    #[test]
    fn renamed_target() {
        let pkg_id = cargo_metadata::PackageId { repr: String::from("42") };
        let version = Version::new(0, 1, 0);

        let mut project_root = std::env::temp_dir();
        project_root.push(Path::new("renamed_target"));
        std::fs::write(&project_root, "").expect("write to temp file");

        let target = GnTarget::new(
            &pkg_id,
            "test_target",
            "test_package",
            Edition::E2018,
            Utf8Path::from_path(project_root.as_path()).unwrap(),
            &version,
            GnRustType::Binary,
            &[],
            false,
            HashMap::new(),
        );

        let outname = Some("rainbow_binary");
        let mut output = vec![];
        write_rule(
            &mut output,
            &target,
            std::env::temp_dir().as_path(),
            None,
            None,
            outname,
            false,
            false,
            Some("renamed_rule"),
            false,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output,
            r#"renamed_rule("test_package-test_target-v0_1_0") {
  crate_name = "test_target"
  crate_root = "//renamed_target"
  output_name = "rainbow_binary"
  
  deps = []

  rustenv = []

  rustflags = ["--cap-lints=allow","--edition=2018","-Cmetadata=bf8f4a806276c599","-Cextra-filename=-bf8f4a806276c599"]

  
  visibility = [":*"]

  
  applicable_licenses = []

}

"#
        );
    }
}
