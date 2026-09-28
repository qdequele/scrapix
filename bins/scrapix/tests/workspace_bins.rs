//! Every binary target in the workspace must have a unique name.
//!
//! Two `[[bin]]`s with the same name write to the same
//! `target/<profile>/<name>` path, and whichever links last wins. The
//! Dockerfile builds the whole workspace and ships `target/release/scrapix`
//! as the `scrapix-all` image (`ENTRYPOINT ["/app/scrapix", "all"]`), so a
//! collision can silently ship a binary without the `all` subcommand.

use std::collections::BTreeMap;
use std::process::Command;

use serde_json::Value;

#[test]
fn workspace_binary_names_are_unique() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("parse cargo metadata");

    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for package in metadata["packages"].as_array().expect("packages") {
        let package_name = package["name"].as_str().expect("package name");
        for target in package["targets"].as_array().expect("targets") {
            let is_bin = target["kind"]
                .as_array()
                .expect("target kind")
                .iter()
                .any(|kind| kind == "bin");
            if is_bin {
                let bin_name = target["name"].as_str().expect("target name");
                owners
                    .entry(bin_name.to_string())
                    .or_default()
                    .push(package_name.to_string());
            }
        }
    }

    let collisions: Vec<_> = owners
        .iter()
        .filter(|(_, packages)| packages.len() > 1)
        .collect();
    assert!(
        collisions.is_empty(),
        "binary name declared by more than one package: {collisions:?}"
    );
    assert_eq!(
        owners.get("scrapix").map(Vec::as_slice),
        Some(["scrapix".to_string()].as_slice()),
        "the `scrapix` binary must come from the unified `scrapix` package"
    );
}
