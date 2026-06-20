use std::path::Path;

fn main() {
    let assets = [
        "assets/index.html",
        "assets/pkg/rdp_wasm.js",
        "assets/pkg/rdp_wasm_bg.wasm",
    ];
    for asset in &assets {
        if !Path::new(asset).exists() {
            panic!(
                "\n\nERROR: '{}' not found.\n\
                 Do not run `cargo publish -p rustrdp-proxy` directly.\n\
                 Use `make publish` instead, which builds the WASM assets first.\n",
                asset
            );
        }
        println!("cargo:rerun-if-changed={}", asset);
    }
}
