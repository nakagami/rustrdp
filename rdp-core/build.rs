fn main() {
    if std::env::var("CARGO_FEATURE_H264").is_ok() {
        let brew = "/opt/homebrew";
        cc::Build::new()
            .file("src/h264_helper.c")
            .include(format!("{}/include", brew))
            .flag("-w") // suppress warnings
            .compile("h264_helper");

        let lib_dir = format!("{}/lib", brew);
        println!("cargo:rustc-link-search=native={}", lib_dir);
        println!("cargo:rustc-link-lib=dylib=avcodec");
        println!("cargo:rustc-link-lib=dylib=avutil");
        println!("cargo:rustc-link-lib=dylib=swscale");
        println!("cargo:rerun-if-changed=src/h264_helper.c");
        println!("cargo:rerun-if-changed=build.rs");
    }
}
