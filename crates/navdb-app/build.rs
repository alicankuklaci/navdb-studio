//! Embeds the application icon into `navdb.exe` when building for Windows.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon/navdb.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon/navdb.ico")
            .set("ProductName", "NavDB Studio")
            .set("FileDescription", "NavDB Studio");
        // a missing resource compiler (e.g. cross builds without windres) only costs the icon
        if let Err(e) = res.compile() {
            println!("cargo:warning=Windows icon not embedded: {e}");
        }
    }
}
