// Embeds the app icon and version info into isla.exe (Explorer, taskbar, Alt-Tab).
fn main() {
    println!("cargo:rerun-if-changed=assets/isla.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/isla.ico")
            .set("ProductName", "Isla")
            .set("FileDescription", "Isla - Dynamic Island untuk Windows")
            .set("LegalCopyright", "");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon not embedded: {e}");
        }
    }
}
