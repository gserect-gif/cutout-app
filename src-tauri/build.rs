fn main() {
    // Linux: also look for libraries in "../lib" next to the app's binary. The
    // .deb puts libwebgpu_dawn.so (the GPU runtime) in /usr/lib and the AppImage
    // in usr/lib, and both are "../lib" from the binary, so the app finds it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib");
    }
    tauri_build::build()
}
