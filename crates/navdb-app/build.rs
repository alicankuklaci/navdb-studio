//! Embeds the application icon into `navdb.exe` when building for Windows, and
//! compiles the StoreKit/bookmark bridge (macos/NavDBBridge.swift) for the Mac
//! App Store build (`--features appstore`).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon/navdb.ico");
    println!("cargo:rerun-if-changed=macos/NavDBBridge.swift");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "windows" {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon/navdb.ico")
            .set("ProductName", "NavDB Studio")
            .set("FileDescription", "NavDB Studio");
        // a missing resource compiler (e.g. cross builds without windres) only costs the icon
        if let Err(e) = res.compile() {
            println!("cargo:warning=Windows icon not embedded: {e}");
        }
    }
    if os == "macos" && std::env::var_os("CARGO_FEATURE_APPSTORE").is_some() {
        swift_bridge();
    }
}

fn swift_bridge() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        _ => "x86_64",
    };
    let lib = out.join("libnavdbbridge.a");
    let status = Command::new("xcrun")
        .args(["swiftc", "-parse-as-library", "-emit-library", "-static", "-O", "-swift-version", "5"])
        .args(["-module-name", "NavDBBridge", "-target", &format!("{arch}-apple-macos12.0")])
        .arg("-module-cache-path")
        .arg(out.join("swift-cache"))
        .arg("-o")
        .arg(&lib)
        .arg("macos/NavDBBridge.swift")
        .status()
        .expect("running swiftc (install Xcode)");
    assert!(status.success(), "swiftc failed");

    // the Swift runtime ships with macOS 12+; the compatibility shims live in the toolchain
    let sdk = String::from_utf8(Command::new("xcrun").args(["--sdk", "macosx", "--show-sdk-path"]).output().unwrap().stdout).unwrap();
    let swiftc = String::from_utf8(Command::new("xcrun").args(["--find", "swiftc"]).output().unwrap().stdout).unwrap();
    let toolchain = PathBuf::from(swiftc.trim()).parent().unwrap().parent().unwrap().join("lib/swift/macosx");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-search=native={}/usr/lib/swift", sdk.trim());
    println!("cargo:rustc-link-search=native={}", toolchain.display());
    println!("cargo:rustc-link-lib=static=navdbbridge");
    println!("cargo:rustc-link-lib=framework=StoreKit");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
