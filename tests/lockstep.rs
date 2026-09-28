//! Issue #44: publishable crates must be version-lockstep
//! and every internal path-dependency's version req must equal that version.

use std::process::Command;

#[test]
fn publishable_crates_are_version_lockstep() {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .expect("cargo metadata");
    assert!(out.status.success(), "cargo metadata failed");
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let pkgs: Vec<&serde_json::Value> = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["publish"].as_array().is_none())
        .collect();
    assert!(!pkgs.is_empty(), "no publishable crates found?");

    let versions: Vec<(String, String)> = pkgs
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap().into(),
                p["version"].as_str().unwrap().into(),
            )
        })
        .collect();
    let first = versions[0].1.clone();
    let drift: Vec<String> = versions
        .iter()
        .filter(|(_, v)| *v != first)
        .map(|(n, v)| format!("{n}={v}"))
        .collect();
    assert!(
        drift.is_empty(),
        "publishable crates not lockstep (baseline {first}): {}",
        drift.join(", ")
    );

    // Internal path deps must require exactly the lockstep version.
    let names: Vec<String> = versions.iter().map(|(n, _)| n.clone()).collect();
    let mut bad = Vec::new();
    for p in &pkgs {
        let from = p["name"].as_str().unwrap();
        for d in p["dependencies"].as_array().unwrap() {
            if d.get("path").is_none() {
                continue;
            }
            let dep = d["name"].as_str().unwrap();
            if !names.contains(&dep.to_string()) {
                continue; // e.g. tui-pty-harness (publish = false)
            }
            let req = d["req"].as_str().unwrap();
            let is_dev = d["kind"].as_str() == Some("dev");
            // Non-dev path deps must be pinned exactly (`=V`); dev-deps may be `*`.
            let expected = format!("={first}");
            if req != expected && !(is_dev && req == "*") {
                bad.push(format!("{from} -> {dep} req={req} (expected {expected})"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "internal path-dep version reqs drifted: {}",
        bad.join("; ")
    );
}
