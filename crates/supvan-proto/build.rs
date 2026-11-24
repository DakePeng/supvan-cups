fn main() {
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rerun-if-changed=macos/rfcomm_shim.m");
        cc::Build::new()
            .file("macos/rfcomm_shim.m")
            .flag("-fobjc-arc")
            .flag("-Wno-deprecated-declarations")
            .compile("svrfcomm");
        println!("cargo:rustc-link-lib=framework=IOBluetooth");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }
}
