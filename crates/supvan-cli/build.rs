fn main() {
    // macOS: embed Info.plist so TCC can prompt for Bluetooth access.
    #[cfg(target_os = "macos")]
    {
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("macos/Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
