// Copyright 2016-2019 Cargo-Bundle developers <https://github.com/burtonageo/cargo-bundle>
// Copyright 2019-2022 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

// A macOS application bundle package is laid out like:
//
// foobar.app    # Actually a directory
//     Contents      # A further subdirectory
//         Info.plist     # An xml file containing the app's metadata
//         MacOS          # A directory to hold executable binary files
//             foobar          # The main binary executable of the app
//             foobar_helper   # A helper application, possibly providing a CLI
//         Resources      # Data files such as images, sounds, translations and nib files
//             en.lproj        # Folder containing english translation strings/data
//         Frameworks     # A directory containing private frameworks (shared libraries)
//         ...            # Any other optional files the developer wants to place here
//
// See https://developer.apple.com/go/?id=bundle-structure for a full
// explanation.
//
// Currently, cargo-bundle does not support Frameworks, nor does it support placing arbitrary
// files into the `Contents` directory of the bundle.

use super::{
  super::common,
  icon::create_icns_file,
  sign::{notarize, notarize_auth_args, sign},
};
use crate::{bundle::common::CommandExt, Settings};

use anyhow::Context;
use log::{info, warn};

use std::{
  fs,
  path::{Path, PathBuf},
  process::Command,
};

/// Bundles the project.
/// Returns a vector of PathBuf that shows where the .app was created.
pub fn bundle_project(settings: &Settings) -> crate::Result<Vec<PathBuf>> {
  // we should use the bundle name (App name) as a MacOS standard.
  // version or platform shouldn't be included in the App name.
  let app_product_name = format!("{}.app", settings.product_name());

  let app_bundle_path = settings
    .project_out_directory()
    .join("bundle/macos")
    .join(&app_product_name);

  info!(action = "Bundling"; "{} ({})", app_product_name, app_bundle_path.display());

  if app_bundle_path.exists() {
    fs::remove_dir_all(&app_bundle_path)
      .with_context(|| format!("Failed to remove old {}", app_product_name))?;
  }
  let bundle_directory = app_bundle_path.join("Contents");
  fs::create_dir_all(&bundle_directory).with_context(|| {
    format!(
      "Failed to create bundle directory at {:?}",
      bundle_directory
    )
  })?;

  let resources_dir = bundle_directory.join("Resources");
  let bin_dir = bundle_directory.join("MacOS");
  let frameworks_dir = bundle_directory.join("Frameworks");

  let bundle_icon_file: Option<PathBuf> =
    { create_icns_file(&resources_dir, settings).with_context(|| "Failed to create app icon")? };

  create_info_plist(&bundle_directory, bundle_icon_file, settings)
    .with_context(|| "Failed to create Info.plist")?;

  copy_frameworks_to_bundle(&bundle_directory, settings)
    .with_context(|| "Failed to bundle frameworks")?;

  settings.copy_resources(&resources_dir)?;

  settings
    .copy_binaries(&bin_dir)
    .with_context(|| "Failed to copy external binaries")?;

  copy_binaries_to_bundle(&bundle_directory, settings)?;

  update_dylibs_rpaths(
    settings,
    bin_dir.join(settings.product_name()),
    frameworks_dir,
  )?;

  // tell main binary where to look for dependencies (in Frameworks)
  install_name_tool(
    "add_rpath",
    vec![String::from("@loader_path/../Frameworks")],
    bin_dir.join(settings.product_name()),
  )?;

  if let Some(identity) = &settings.macos().signing_identity {
    // sign application
    sign(app_bundle_path.clone(), identity, settings, true)?;
    // notarization is required for distribution
    match notarize_auth_args() {
      Ok(args) => {
        notarize(app_bundle_path.clone(), args, settings)?;
      }
      Err(e) => {
        warn!("skipping app notarization, {}", e.to_string());
      }
    }
  }

  Ok(vec![app_bundle_path])
}

fn update_dylibs_rpaths(
  settings: &Settings,
  bin_path: PathBuf,
  frameworks_path: PathBuf,
) -> crate::Result<()> {
  let frameworks = settings
    .macos()
    .frameworks
    .as_ref()
    .cloned()
    .unwrap_or_default();

  for framework in frameworks.iter() {
    let lib_path = PathBuf::from(framework);
    if (framework.ends_with(".dylib") || framework.ends_with(".so") || framework.ends_with(".so.2"))
      && lib_path.exists()
    {
      let lib_name = lib_path
        .file_name()
        .expect("Couldn't get framework filename")
        .to_str()
        .expect("Couldn't extract framework filename");

      // change id of current lib
      install_name_tool(
        "id",
        vec![format!("@rpath/{}", lib_name)],
        frameworks_path.join(lib_name),
      )?;

      // tell current lib to use its own directory (Frameworks) to look for dependencies
      install_name_tool(
        "add_rpath",
        vec![String::from("@loader_path")],
        frameworks_path.join(lib_name),
      );

      // tell all dependant libraries to look for the lib in @rpath (which now includes their own directory)
      for dependant_framework in frameworks.iter() {
        let dependant_lib_path = PathBuf::from(dependant_framework);
        if (dependant_framework.ends_with(".dylib")
          || framework.ends_with(".so")
          || framework.ends_with(".so.2"))
          && dependant_lib_path.exists()
        {
          let dependant_lib_name = dependant_lib_path
            .file_name()
            .expect("Couldn't get framework filename")
            .to_str()
            .expect("Couldn't extract framework filename");

          install_name_tool(
            "change",
            vec![
              lib_path.display().to_string(),
              format!("@rpath/{}", lib_name),
            ],
            frameworks_path.join(dependant_lib_name),
          )?;
        }
      }

      // tell main binary to look for lib in @rpath
      install_name_tool(
        "change",
        vec![
          lib_path.display().to_string(),
          format!("@rpath/{}", lib_name),
        ],
        bin_path.clone(),
      )?;
    }
  }
  Ok(())
}

fn install_name_tool(action: &str, args: Vec<String>, file: PathBuf) -> crate::Result<()> {
  info!(action = "Running"; "install_name_tool -{} {} {}", action, args.join(" "), file.display());

  Command::new("install_name_tool")
    .arg(format!("-{}", action))
    .args(args)
    .arg(file)
    .output_ok()
    .context("failed to run install_name_tool")?;
  Ok(())
}

// Copies the app's binaries to the bundle.
fn copy_binaries_to_bundle(bundle_directory: &Path, settings: &Settings) -> crate::Result<()> {
  let dest_dir = bundle_directory.join("MacOS");
  for bin in settings.binaries() {
    let bin_path = settings.binary_path(bin);
    common::copy_file(&bin_path, &dest_dir.join(bin.name()))
      .with_context(|| format!("Failed to copy binary from {:?}", bin_path))?;
  }
  Ok(())
}

// Creates the Info.plist file.
fn create_info_plist(
  bundle_dir: &Path,
  bundle_icon_file: Option<PathBuf>,
  settings: &Settings,
) -> crate::Result<()> {
  let format = time::format_description::parse("[year][month][day].[hour][minute][second]")
    .map_err(time::error::Error::from)?;
  let build_number = time::OffsetDateTime::now_utc()
    .format(&format)
    .map_err(time::error::Error::from)?;

  let mut plist = plist::Dictionary::new();
  plist.insert("CFBundleDevelopmentRegion".into(), "English".into());
  plist.insert("CFBundleDisplayName".into(), settings.product_name().into());
  plist.insert(
    "CFBundleExecutable".into(),
    settings.main_binary_name().into(),
  );
  if let Some(path) = bundle_icon_file {
    plist.insert(
      "CFBundleIconFile".into(),
      path
        .file_name()
        .expect("No file name")
        .to_string_lossy()
        .into_owned()
        .into(),
    );
  }
  plist.insert(
    "CFBundleIdentifier".into(),
    settings.bundle_identifier().into(),
  );
  plist.insert("CFBundleInfoDictionaryVersion".into(), "6.0".into());
  plist.insert("CFBundleName".into(), settings.product_name().into());
  plist.insert("CFBundlePackageType".into(), "APPL".into());
  plist.insert(
    "CFBundleShortVersionString".into(),
    settings.version_string().into(),
  );
  plist.insert("CFBundleVersion".into(), build_number.into());
  plist.insert("CSResourcesFileMapped".into(), true.into());
  if let Some(category) = settings.app_category() {
    plist.insert(
      "LSApplicationCategoryType".into(),
      category.macos_application_category_type().into(),
    );
  }
  if let Some(version) = settings.macos().minimum_system_version.clone() {
    plist.insert("LSMinimumSystemVersion".into(), version.into());
  }
  plist.insert("LSRequiresCarbon".into(), true.into());
  plist.insert("NSHighResolutionCapable".into(), true.into());
  if let Some(copyright) = settings.copyright_string() {
    plist.insert("NSHumanReadableCopyright".into(), copyright.into());
  }

  if let Some(exception_domain) = settings.macos().exception_domain.clone() {
    let mut security = plist::Dictionary::new();
    let mut domain = plist::Dictionary::new();
    domain.insert("NSExceptionAllowsInsecureHTTPLoads".into(), true.into());
    domain.insert("NSIncludesSubdomains".into(), true.into());

    let mut exception_domains = plist::Dictionary::new();
    exception_domains.insert(exception_domain, domain.into());
    security.insert("NSExceptionDomains".into(), exception_domains.into());
    plist.insert("NSAppTransportSecurity".into(), security.into());
  }

  if let Some(user_plist_path) = &settings.macos().info_plist_path {
    let user_plist = plist::Value::from_file(user_plist_path)?;
    if let Some(dict) = user_plist.into_dictionary() {
      for (key, value) in dict {
        plist.insert(key, value);
      }
    }
  }

  plist::Value::Dictionary(plist).to_file_xml(bundle_dir.join("Info.plist"))?;

  Ok(())
}

// Copies the framework under `{src_dir}/{framework}.framework` to `{dest_dir}/{framework}.framework`.
fn copy_framework_from(dest_dir: &Path, framework: &str, src_dir: &Path) -> crate::Result<bool> {
  let src_name = format!("{}.framework", framework);
  let src_path = src_dir.join(&src_name);
  if src_path.exists() {
    common::copy_dir(&src_path, &dest_dir.join(&src_name))?;
    Ok(true)
  } else {
    Ok(false)
  }
}

// Copies the macOS application bundle frameworks to the .app
fn copy_frameworks_to_bundle(bundle_directory: &Path, settings: &Settings) -> crate::Result<()> {
  let frameworks = settings
    .macos()
    .frameworks
    .as_ref()
    .cloned()
    .unwrap_or_default();
  if frameworks.is_empty() {
    return Ok(());
  }
  let dest_dir = bundle_directory.join("Frameworks");
  fs::create_dir_all(&bundle_directory)
    .with_context(|| format!("Failed to create Frameworks directory at {:?}", dest_dir))?;
  for framework in frameworks.iter() {
    if framework.ends_with(".framework") {
      let src_path = PathBuf::from(framework);
      let src_name = src_path
        .file_name()
        .expect("Couldn't get framework filename");
      common::copy_dir(&src_path, &dest_dir.join(&src_name))?;
      continue;
    } else if framework.ends_with(".dylib")
      || framework.ends_with(".so")
      || framework.ends_with(".so.2")
    {
      let src_path = PathBuf::from(framework);
      let src_name = src_path
        .file_name()
        .expect("Couldn't get framework filename")
        .to_str()
        .expect("Could not convert to string");
      let dest_path = &dest_dir.join(&src_name);
      if !src_path.exists() {
        info!("skipping file {} as it does not exist", &src_path.display());
      } else if dest_path.exists() {
        info!("file {} already exists", &dest_path.display());
      } else {
        info!(
          "copying file from {} to {}",
          &src_path.display(),
          &dest_path.display()
        );
        common::copy_file(&src_path, &dest_path)?;
      }
      continue;
    } else if framework.contains('/') {
      return Err(crate::Error::GenericError(format!(
        "Framework path should have .framework extension: {}",
        framework
      )));
    }
    if let Some(home_dir) = dirs_next::home_dir() {
      if copy_framework_from(&dest_dir, framework, &home_dir.join("Library/Frameworks/"))? {
        continue;
      }
    }
    if copy_framework_from(&dest_dir, framework, &PathBuf::from("/Library/Frameworks/"))?
      || copy_framework_from(
        &dest_dir,
        framework,
        &PathBuf::from("/Network/Library/Frameworks/"),
      )?
    {
      continue;
    }
    return Err(crate::Error::GenericError(format!(
      "Could not locate framework: {}",
      framework
    )));
  }
  Ok(())
}
