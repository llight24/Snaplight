// Embeds icon/icon.ico into snaplight-plugin-host.exe; see
// scripts/build/app_icon.rs.
include!("../../scripts/build/app_icon.rs");

fn main() {
    embed_app_icon();
}
