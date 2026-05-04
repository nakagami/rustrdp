use std::process::Command;

fn main() {
    // ── SDL2 setup ────────────────────────────────────────────────────────────
    // SDL2 path from Homebrew
    let output = Command::new("brew")
        .arg("--prefix")
        .arg("sdl2")
        .output()
        .expect("Failed to get SDL2 path from Homebrew");

    let sdl2_path = String::from_utf8(output.stdout)
        .expect("Failed to parse SDL2 path")
        .trim()
        .to_string();

    let sdl2_lib_path = format!("{}/lib", sdl2_path);
    let _sdl2_include_path = format!("{}/include", sdl2_path);

    println!("cargo:rustc-link-search=native={}", sdl2_lib_path);
    println!("cargo:rustc-link-search=native={}/SDL2.framework/Frameworks", sdl2_path);

    // SDL2_ttf
    let output = Command::new("brew")
        .arg("--prefix")
        .arg("sdl2_ttf")
        .output()
        .expect("Failed to get SDL2_ttf path from Homebrew");

    let sdl2_ttf_path = String::from_utf8(output.stdout)
        .expect("Failed to parse SDL2_ttf path")
        .trim()
        .to_string();

    let sdl2_ttf_lib_path = format!("{}/lib", sdl2_ttf_path);
    println!("cargo:rustc-link-search=native={}", sdl2_ttf_lib_path);

    // Use pkg-config if available
    pkg_config::probe_library("sdl2").ok();
    pkg_config::probe_library("SDL2_ttf").ok();
    // H.264 decode is handled by the `openh264` Rust crate (no FFmpeg needed).
}
