fn main() {
    tauri_build::build();
    // tauri-build links the Windows Common-Controls v6 manifest only into bins,
    // so debug `cargo test --lib` harnesses fail at load time (0xc0000139) without it.
    if std::env::var("PROFILE").as_deref() == Ok("debug") {
        if let Ok(out_dir) = std::env::var("OUT_DIR") {
            let out = std::path::Path::new(&out_dir);
            for candidate in ["libresource.a", "resource.lib"] {
                let resource = out.join(candidate);
                if resource.is_file() {
                    println!("cargo:rustc-link-arg={}", resource.display());
                }
            }
        }
    }
}
