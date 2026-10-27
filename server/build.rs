// Compiles bridge/simbridge.m; the binary is embedded in simpoold and unpacked at runtime.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bridge/simbridge.m");
    println!("cargo:rerun-if-changed={}", src.display());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("simbridge");
    let status = Command::new("xcrun")
        .args(["clang", "-O2", "-fobjc-arc", "-Wall", "-mmacosx-version-min=14.0"])
        .args(["AppKit", "CoreImage", "CoreGraphics", "Foundation", "ImageIO", "IOSurface"].iter().flat_map(|f| ["-framework", f]))
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .expect("running xcrun clang (needs Xcode command line tools)");
    assert!(status.success(), "compiling simbridge.m failed");
}
