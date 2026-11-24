use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(agility_sdk)");
    println!("cargo::rerun-if-changed=build.rs");

    // OUT_DIR is <target>/<profile>/build/<package>-<hash>/out.
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let Some(profile) = out.ancestors().nth(3) else {
        return;
    };
    let core = profile.join("D3D12").join("D3D12Core.dll");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if msvc && core.is_file() {
        println!("cargo::rerun-if-changed={}", core.display());
        println!("cargo::rustc-cfg=agility_sdk");
        println!("cargo::rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo::rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
    }
    // `cargo xtask agility` touches this script after installing the SDK.
    // Watching a missing DLL would make Cargo run the script on every build.
}
