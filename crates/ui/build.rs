fn main() {
    println!("cargo:rerun-if-env-changed=SNAPLIGHT_MINGW_LIB");
    if let Ok(path) = std::env::var("SNAPLIGHT_MINGW_LIB") {
        println!("cargo:rustc-link-search=native={path}");
    }
    slint_build::compile("ui/toolbar.slint").expect("compile Slint UI");
}
