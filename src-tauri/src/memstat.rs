//! Lightweight process memory reporting, used only to add real numbers to
//! the `[perf]` diagnostic logging while investigating RAM usage that grows
//! across repeated background-removal operations. Not a general-purpose
//! utility — narrowly scoped to this one diagnostic need.
//!
//! Implementation follows the confirmed-working pattern from the
//! `memory-stats` crate's own Windows backend (using `GetProcessMemoryInfo`
//! via `windows-sys`), rather than inventing the FFI call from scratch.
//!
//! Output goes to BOTH stderr and a plain-text log file. The release build
//! is a Windows GUI-subsystem app, which has no console attached, so stderr
//! alone is not reliably visible when the installed app is launched
//! normally. The log file (`cutout-perf.log` in the system temp folder,
//! i.e. `%TEMP%\cutout-perf.log`) can always be opened and sent as-is.

use std::io::Write;
use std::path::PathBuf;

fn log_path() -> PathBuf {
    std::env::temp_dir().join("cutout-perf.log")
}

/// Starts a fresh log for this app run so the file only ever contains one
/// session and can't grow without bound across launches.
pub fn reset_log() {
    let _ = std::fs::write(log_path(), "");
    log_line("--- Cutout session started ---");
}

/// Appends one line to the log file (best-effort: a failed write is ignored
/// so diagnostics can never break the app) and mirrors it to stderr.
pub fn log_line(line: &str) {
    eprintln!("{line}");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(windows)]
pub fn current_process_memory_mb() -> Option<(u64, u64)> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::uninit();

    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            counters.as_mut_ptr(),
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };

    if ok == 0 {
        return None;
    }

    // SAFETY: `ok != 0` confirms GetProcessMemoryInfo filled `counters`.
    let counters = unsafe { counters.assume_init() };

    // WorkingSetSize: physical RAM currently in use by this process (what
    // Task Manager's "Memory" column shows). PagefileUsage: committed
    // virtual memory backed by the page file, a useful second signal for
    // whether growth is genuinely resident or just reserved/committed.
    let working_set_mb = counters.WorkingSetSize as u64 / (1024 * 1024);
    let pagefile_mb = counters.PagefileUsage as u64 / (1024 * 1024);

    Some((working_set_mb, pagefile_mb))
}

#[cfg(not(windows))]
pub fn current_process_memory_mb() -> Option<(u64, u64)> {
    // Only implemented for Windows, since that's the platform this
    // diagnostic investigation is specifically about. Returns None
    // elsewhere rather than a fake/zero value.
    None
}

/// Logs current process memory with a stage label, or a plain "unavailable"
/// note on non-Windows/failure, so the log always has a consistent line to
/// look for regardless of platform.
pub fn log_memory(stage: &str) {
    match current_process_memory_mb() {
        Some((working_set_mb, pagefile_mb)) => {
            log_line(&format!(
                "[perf][mem] {stage}: working set {working_set_mb} MB, committed {pagefile_mb} MB"
            ));
        }
        None => {
            log_line(&format!("[perf][mem] {stage}: unavailable on this platform"));
        }
    }
}
