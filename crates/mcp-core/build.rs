// SPDX-License-Identifier: GPL-3.0-or-later
//! Stamps build provenance into the crate so `get_runtime_provenance` can
//! report the source revision and build target of the running binary.
//! Values are best-effort: an absent git checkout yields an empty revision,
//! which the runtime reports as `null` with a reason.
use std::process::Command;

fn source_revision() -> String {
    if let Ok(sha) = std::env::var("GITHUB_SHA") {
        if !sha.trim().is_empty() {
            return sha.trim().to_owned();
        }
    }
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    for path in ["../../.git/HEAD", "../../.git/refs/heads"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!(
        "cargo:rustc-env=SIMRACECENTER_SOURCE_REVISION={}",
        source_revision()
    );
    println!(
        "cargo:rustc-env=SIMRACECENTER_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=SIMRACECENTER_BUILD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_default()
    );
}
