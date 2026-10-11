// Shared build-script logic that embeds `icon/icon.ico` into every Snaplight
// executable as a Win32 icon resource.
//
// Every binary crate includes this file from its own `build.rs`:
//
//     include!("../../scripts/build/app_icon.rs");
//
//     fn main() {
//         embed_app_icon();
//     }
//
// The icon has to be a real resource: Explorer, the taskbar and Alt-Tab take it
// from the executable, so setting it only at runtime cannot cover the file icon.
// The workspace deliberately has no resource-compiler crate dependency, so this
// script drives the toolchain's own compiler instead: the Windows SDK `rc.exe`
// for MSVC targets and `windres` for GNU targets. A host without either keeps
// building and only warns, so cross-compilation is not blocked by an icon.
//
// The resource id is 1; `crates/core/src/native.rs` loads the same id for the
// notification-area icon, so both must stay in sync.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Compiles `icon/icon.ico` and asks the linker to include the result in the
/// crate's binaries.
pub fn embed_app_icon() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let icon = manifest.join("../../icon/icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    // Only Windows executables carry a Win32 icon resource.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if !icon.is_file() {
        println!(
            "cargo:warning={} is missing; no icon resource is embedded",
            icon.display()
        );
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let script = out.join("app-icon.rc");
    // rc.exe reads `\\` in a string literal as one backslash.
    let escaped = icon.to_string_lossy().replace('\\', "\\\\");
    if let Err(error) = std::fs::write(&script, format!("1 ICON \"{escaped}\"\n")) {
        println!("cargo:warning=cannot write {}: {error}", script.display());
        return;
    }
    match compile_resource(&script, &out) {
        Some(link_input) => println!("cargo:rustc-link-arg-bins={}", link_input.display()),
        None => println!(
            "cargo:warning=no resource compiler (rc.exe or windres) found; the app icon is not embedded"
        ),
    }
}

/// Compiles the resource script into a linker input with the compiler that
/// matches the target environment.
fn compile_resource(script: &Path, out: &Path) -> Option<PathBuf> {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let rc = find_sdk_rc()?;
        let resource = out.join("app-icon.res");
        let status = Command::new(rc)
            .arg("/nologo")
            .arg(format!("/fo{}", resource.display()))
            .arg(script)
            .status()
            .ok()?;
        return status.success().then_some(resource);
    }
    // GNU targets: windres emits a COFF object that the linker accepts.
    let windres = find_in_path("windres").or_else(|| find_in_path("llvm-windres"))?;
    let object = out.join("app-icon.o");
    let status = Command::new(windres)
        .args(["-O", "coff", "-i"])
        .arg(script)
        .arg("-o")
        .arg(&object)
        .status()
        .ok()?;
    status.success().then_some(object)
}

/// Finds `rc.exe`: on `PATH`, at the location the SDK environment variables
/// name (a plain `cargo build` never runs `vcvars`), or in the newest installed
/// SDK under `%ProgramFiles(x86)%\Windows Kits\10\bin`.
fn find_sdk_rc() -> Option<PathBuf> {
    if let Some(found) = find_in_path("rc") {
        return Some(found);
    }
    if let (Ok(dir), Ok(version)) = (
        std::env::var("WindowsSdkDir"),
        std::env::var("WindowsSDKVersion"),
    ) {
        let candidate = Path::new(&dir)
            .join("bin")
            .join(version.trim_end_matches('\\'))
            .join("x64")
            .join("rc.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let kits = std::env::var("ProgramFiles(x86)").ok()?;
    let bin = Path::new(&kits).join("Windows Kits").join("10").join("bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(bin)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join("x64").join("rc.exe").is_file())
        .collect();
    // SDK directory names are versions, which sort correctly as strings.
    versions.sort();
    versions.pop().map(|dir| dir.join("x64").join("rc.exe"))
}

/// Looks for `name.exe` in the directories listed by `PATH`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(format!("{name}.exe")))
        .find(|candidate| candidate.is_file())
}
