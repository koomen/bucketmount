use std::path::Path;
use std::process::Command;

fn main() {
    ensure_rclone();
    tauri_build::build()
}

/// Tauri's `externalBin` requires `binaries/rclone-<target>` to exist even for
/// a plain `cargo build`. Fetch it on a fresh checkout so that works.
fn ensure_rclone() {
    let target = std::env::var("TARGET").unwrap();
    let bin = format!("binaries/rclone-{target}");
    println!("cargo:rerun-if-changed={bin}");
    if Path::new(&bin).is_file() {
        return;
    }
    let ok = Command::new("bash")
        .args(["scripts/fetch-rclone.sh", &target])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        panic!("{bin} is missing and scripts/fetch-rclone.sh failed; run it by hand to see why");
    }
}
