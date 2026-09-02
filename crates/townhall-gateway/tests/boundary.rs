//! A19: the crate graph is the trust posture — verified on the RESOLVED graph.
//!
//! A manifest scan reads what a Cargo.toml says; `cargo metadata` reads what the
//! resolver actually linked, which is what a transitive dependency would show up
//! in. The difference matters exactly once, and that once is the hole.

use std::process::Command;

fn resolved_dependencies(package: &str, kinds: &[&str]) -> Vec<String> {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1"])
        .output()
        .expect("cargo metadata");
    assert!(output.status.success());
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("metadata json");

    let packages = metadata["packages"].as_array().expect("packages");
    let subject = packages
        .iter()
        .find(|p| p["name"] == package)
        .unwrap_or_else(|| panic!("{package} not in the workspace"));
    subject["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .filter(|dep| {
            let kind = dep["kind"].as_str().unwrap_or("normal");
            kinds.contains(&kind)
        })
        .map(|dep| dep["name"].as_str().expect("name").to_owned())
        .collect()
}

/// The gateway's NORMAL dependencies may not name the server's insides.
///
/// Dev-dependencies are exempt here, deliberately: these tests spawn the real
/// server, which is the whole point of them. The exemption is why the channel's
/// check below covers both kinds — it has no such need.
#[test]
fn a19_the_gateway_cannot_name_the_servers_insides() {
    let forbidden = [
        "townhall-service",
        "townhall-store",
        "townhall-http",
        "townhall-domain",
        "bld-kernel",
        "sqlx",
    ];
    let deps = resolved_dependencies("townhall-gateway", &["normal"]);
    for name in forbidden {
        assert!(
            !deps.iter().any(|dep| dep == name),
            "townhall-gateway must not depend on {name}; its only route to a \
             booking is a socket, and this dependency would be a second one"
        );
    }
}

/// The channel's check covers dev-dependencies TOO.
///
/// Its tests need no server, so unlike the gateway it gets no exemption — with
/// one, a `#[cfg(test)]` module could reach straight into the store, and the
/// trust posture would hold everywhere except in the code that verifies it.
#[test]
fn a19_the_channel_cannot_name_anything_with_power_even_in_tests() {
    let forbidden = [
        "townhall-gateway",
        "townhall-service",
        "townhall-store",
        "townhall-http",
        "bld-kernel",
        "sqlx",
        "reqwest",
    ];
    let deps = resolved_dependencies("townhall-channel", &["normal", "dev", "build"]);
    for name in forbidden {
        assert!(
            !deps.iter().any(|dep| dep == name),
            "townhall-channel must not depend on {name} in ANY dependency kind"
        );
    }
}
