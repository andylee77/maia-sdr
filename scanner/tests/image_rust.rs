//! No package the unit's build compiles needs a newer Rust than the Tezuka image builds with,
//! which is the manifest's `rust-version`. A PC's Rust is newer and builds such a package
//! without a word; the image's cargo then refuses the whole build ("rustc 1.86.0 is not
//! supported by the following packages"). This reads what `Cargo.lock` resolves to for the
//! unit's target, as cargo itself tells it.

use std::process::Command;

/// `1.86` or `1.88.0` as numbers to compare.
fn version(text: &str) -> (u32, u32, u32) {
    let mut parts = text.split('.').map(|p| p.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

#[test]
fn every_package_builds_with_the_images_rust() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked", "--filter-platform", "armv7-unknown-linux-gnueabihf"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(out.status.success(), "cargo metadata: {}", String::from_utf8_lossy(&out.stderr));
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).expect("cargo metadata's JSON");
    let packages = meta["packages"].as_array().expect("packages");
    let image = packages
        .iter()
        .find(|p| p["name"] == "scanner")
        .and_then(|p| p["rust_version"].as_str())
        .expect("the manifest says which Rust the image builds with (rust-version)");
    // What the unit's build compiles: the resolved graph for its target, build scripts' needs too.
    let built: std::collections::HashSet<&str> = meta["resolve"]["nodes"].as_array().expect("nodes").iter().filter_map(|n| n["id"].as_str()).collect();
    let newer: Vec<String> = packages
        .iter()
        .filter(|p| p["id"].as_str().is_some_and(|id| built.contains(id)))
        .filter_map(|p| {
            let needs = p["rust_version"].as_str()?;
            (version(needs) > version(image)).then(|| format!("{} {} needs Rust {needs}", p["name"].as_str().unwrap_or("?"), p["version"].as_str().unwrap_or("?")))
        })
        .collect();
    assert!(built.len() > 50, "the unit's build was not resolved: {} packages", built.len());
    assert!(
        newer.is_empty(),
        "the image builds with Rust {image}, and these need a newer one: {newer:?}. Lock each to a version that does not (`cargo update <name>`: .cargo/config.toml has cargo pick one)"
    );
}

#[test]
fn versions_compare_as_numbers() {
    assert!(version("1.88.0") > version("1.86"));
    assert!(version("1.100") > version("1.86"));
    assert_eq!(version("1.86"), version("1.86.0"));
}
