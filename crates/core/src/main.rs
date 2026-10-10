#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[cfg(windows)]
mod native;
fn main() {
    #[cfg(windows)]
    if let Err(e) = native::run() {
        snaplight_platform::native::show_error(&e.to_string());
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    eprintln!("Snaplight currently supports Windows only");
}
