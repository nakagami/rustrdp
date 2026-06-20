use std::env;
use std::process::Command;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if target_os == "macos" {
        // ── SDL2 setup (macOS / Homebrew) ────────────────────────────────────
        if let Ok(output) = Command::new("brew").arg("--prefix").arg("sdl2").output() {
            let sdl2_path = String::from_utf8(output.stdout)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !sdl2_path.is_empty() {
                println!("cargo:rustc-link-search=native={}/lib", sdl2_path);
                println!(
                    "cargo:rustc-link-search=native={}/SDL2.framework/Frameworks",
                    sdl2_path
                );
            }
        }

        // SDL2_ttf (macOS / Homebrew)
        if let Ok(output) = Command::new("brew").arg("--prefix").arg("sdl2_ttf").output() {
            let sdl2_ttf_path = String::from_utf8(output.stdout)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !sdl2_ttf_path.is_empty() {
                println!("cargo:rustc-link-search=native={}/lib", sdl2_ttf_path);
            }
        }
    }

    // Use pkg-config on all platforms (Linux, macOS, etc.)
    pkg_config::probe_library("sdl2").ok();
    pkg_config::probe_library("SDL2_ttf").ok();
    // H.264 decode is handled by `ffmpeg-next`, which links against the system
    // FFmpeg libraries discovered by pkg-config / the toolchain defaults.
}
