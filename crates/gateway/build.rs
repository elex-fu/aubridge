use std::{env, path::PathBuf, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn object_id(value: Option<String>) -> Option<String> {
    value.filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn main() {
    for key in [
        "AUBRIDGE_BUILD_REVISION",
        "AUBRIDGE_BUILD_TREE",
        "AUBRIDGE_BUILD_MODE",
        "AUBRIDGE_SOURCE_VERIFIED",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    for path in [
        git(&["rev-parse", "--git-path", "HEAD"]),
        git(&["symbolic-ref", "--quiet", "HEAD"])
            .and_then(|reference| git(&["rev-parse", "--git-path", &reference])),
    ]
    .into_iter()
    .flatten()
    {
        let path = PathBuf::from(path);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let pinned_revision = object_id(env::var("AUBRIDGE_BUILD_REVISION").ok());
    let pinned_tree = object_id(env::var("AUBRIDGE_BUILD_TREE").ok());
    let mode = env::var("AUBRIDGE_BUILD_MODE")
        .ok()
        .filter(|mode| matches!(mode.as_str(), "development" | "distribution"))
        .unwrap_or_else(|| "source".into());
    let verified = mode != "source"
        && env::var("AUBRIDGE_SOURCE_VERIFIED").as_deref() == Ok("1")
        && pinned_revision.is_some()
        && pinned_tree.is_some();
    let revision = pinned_revision.or_else(|| object_id(git(&["rev-parse", "HEAD"])));
    let tree = pinned_tree.or_else(|| object_id(git(&["rev-parse", "HEAD^{tree}"])));
    println!(
        "cargo:rustc-env=AUBRIDGE_GIT_REVISION={}",
        revision.unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=AUBRIDGE_GIT_TREE={}",
        tree.unwrap_or_default()
    );
    println!("cargo:rustc-env=AUBRIDGE_BINARY_MODE={mode}");
    println!("cargo:rustc-env=AUBRIDGE_SOURCE_CHECKED={verified}");
    println!(
        "cargo:rustc-env=AUBRIDGE_BINARY_PROFILE={}",
        env::var("PROFILE").unwrap_or_default()
    );
}
