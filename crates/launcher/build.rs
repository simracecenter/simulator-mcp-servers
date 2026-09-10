// SPDX-License-Identifier: GPL-3.0-or-later
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_manifest(new_manifest("SimRaceCenter.Launcher"))
            .expect("unable to embed Windows application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
