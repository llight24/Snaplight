//! Helpers shared by the plugin-host integration tests.
#![cfg(windows)]
use snaplight_domain::Envelope;
use snaplight_plugin_api::RunRequest;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

/// Runs one request through a fresh host process and returns its output.
pub fn run(message: &Envelope<RunRequest>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_snaplight-plugin-host"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "{}",
        serde_json::to_string(message).unwrap()
    )
    .unwrap();
    child.wait_with_output().unwrap()
}

/// Path of the example plugin built next to the host binary. The workspace
/// build produces it; `SNAPLIGHT_EXAMPLE_PLUGIN` overrides the location. Only
/// the GPU transport test loads a real plugin, so other test binaries in this
/// crate do not use this helper.
#[allow(dead_code)]
pub fn example_plugin() -> PathBuf {
    if let Some(path) = std::env::var_os("SNAPLIGHT_EXAMPLE_PLUGIN") {
        return PathBuf::from(path);
    }
    let host = PathBuf::from(env!("CARGO_BIN_EXE_snaplight-plugin-host"));
    let path = host.parent().unwrap().join(format!(
        "snaplight_example_plugin{}",
        std::env::consts::DLL_SUFFIX
    ));
    assert!(
        path.exists(),
        "run `cargo build --workspace` first: {} is missing",
        path.display()
    );
    // `cargo test` rebuilds the test binary but not a cdylib dependency, so a
    // stale library would silently exercise an older plugin ABI.
    let built = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .expect("plugin library timestamp");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("plugins/example/src/lib.rs");
    if let Ok(modified) = fs::metadata(&source).and_then(|metadata| metadata.modified()) {
        assert!(
            built >= modified,
            "run `cargo build --workspace` first: {} is older than {}",
            path.display(),
            source.display()
        );
    }
    path
}
