//! Startup CPU check.
//!
//! The prebuilt ONNX Runtime that the `ort` crate downloads targets the
//! x86-64-v3 instruction level, which requires AVX2. On a CPU without AVX2
//! (for example AMD Kaveri/A-series chips and Intel Sandy/Ivy Bridge) the
//! first AI call dies with an "illegal instruction" crash and no message.
//! Checking up front lets Cutout say what is wrong instead of vanishing.
//!
//! Only x86_64 needs this. Other CPU types (such as Apple Silicon) skip it.

/// True if this CPU can run the bundled ONNX Runtime.
#[cfg(target_arch = "x86_64")]
pub fn cpu_is_supported() -> bool {
    std::is_x86_feature_detected!("avx2")
}

#[cfg(not(target_arch = "x86_64"))]
pub fn cpu_is_supported() -> bool {
    true
}

/// One line describing the CPU features that matter, for the startup log.
#[cfg(target_arch = "x86_64")]
pub fn describe_cpu() -> String {
    format!(
        "[startup] cpu: avx={} avx2={} fma={}",
        std::is_x86_feature_detected!("avx"),
        std::is_x86_feature_detected!("avx2"),
        std::is_x86_feature_detected!("fma"),
    )
}

#[cfg(not(target_arch = "x86_64"))]
pub fn describe_cpu() -> String {
    "[startup] cpu: not x86_64, AVX2 check skipped".to_string()
}

const UNSUPPORTED_TITLE: &str = "Cutout can't run on this computer";

const UNSUPPORTED_MESSAGE: &str = "Cutout's AI engine needs a processor with AVX2 support, and this computer's processor doesn't have it. This is typical of PCs made before about 2013\u{2013}2015.\n\nCutout can't remove backgrounds on this computer. It will run on any newer PC.";

/// Tells the user, in a way that works for a GUI app with no console.
#[cfg(windows)]
pub fn show_unsupported_cpu_error() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let title = wide(UNSUPPORTED_TITLE);
    let message = wide(UNSUPPORTED_MESSAGE);

    // SAFETY: both buffers are NUL-terminated and outlive the call; a null
    // owner window is allowed and makes this a standalone message box.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
pub fn show_unsupported_cpu_error() {
    eprintln!("{UNSUPPORTED_TITLE}: {UNSUPPORTED_MESSAGE}");
}
