//! Finds the graphics adapters in this computer and tells a dedicated graphics
//! card (fast for AI work) from the integrated graphics built into the
//! processor (slower than the CPU for Cutout in our tests). "Auto" GPU mode
//! uses this to switch the GPU on only when there is a dedicated card.
//!
//! Detection runs once and the answer is kept. If anything goes wrong while
//! looking, the adapter is simply treated as not dedicated, which keeps the
//! safe default: the CPU.

use std::sync::OnceLock;

/// A dedicated card needs at least this much video memory to be picked
/// automatically (skipped when the system doesn't report its memory).
const MIN_AUTO_MEMORY_MB: u64 = 2048;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuAdapter {
    /// Name as the operating system reports it.
    pub name: String,
    /// True for a separate graphics card with its own memory, false for
    /// graphics built into the processor.
    pub dedicated: bool,
    /// The card's own memory in MB (0 when unknown).
    pub memory_mb: u64,
    /// Position in the operating system's adapter list. Windows uses it to
    /// point DirectML at the right card on laptops with two GPUs.
    #[serde(skip)]
    pub index: i32,
}

static ADAPTERS: OnceLock<Vec<GpuAdapter>> = OnceLock::new();

/// Every graphics adapter found (software adapters are left out).
pub fn adapters() -> &'static [GpuAdapter] {
    ADAPTERS.get_or_init(detect).as_slice()
}

/// The dedicated card that Auto mode would use: the one with the most memory,
/// or None when this computer only has integrated graphics.
pub fn best_dedicated() -> Option<&'static GpuAdapter> {
    adapters()
        .iter()
        .filter(|a| a.dedicated && (a.memory_mb == 0 || a.memory_mb >= MIN_AUTO_MEMORY_MB))
        .max_by_key(|a| a.memory_mb)
}

#[cfg(target_os = "windows")]
fn detect() -> Vec<GpuAdapter> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Direct3D12::{
        D3D12CreateDevice, ID3D12Device, D3D12_FEATURE_ARCHITECTURE,
        D3D12_FEATURE_DATA_ARCHITECTURE,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

    let mut found = Vec::new();

    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(factory) => factory,
        Err(_) => return found,
    };

    let mut index: u32 = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        let position = index as i32;
        index += 1;

        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        // 2 = DXGI_ADAPTER_FLAG_SOFTWARE ("Microsoft Basic Render Driver").
        // 0x1414 is Microsoft's own vendor id (remote display adapters).
        if desc.Flags & 2 != 0 || desc.VendorId == 0x1414 {
            continue;
        }

        let name_len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..name_len])
            .trim()
            .to_string();
        let memory_mb = (desc.DedicatedVideoMemory as u64) / (1024 * 1024);

        // Integrated graphics share the computer's memory ("UMA"); a dedicated
        // card has its own. Windows tells us through Direct3D 12, which is
        // also what DirectML runs on. If the adapter can't do Direct3D 12,
        // DirectML can't use it either, so it doesn't count as dedicated.
        let mut device: Option<ID3D12Device> = None;
        let created = unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device) };
        let shares_memory = match (created, device) {
            (Ok(()), Some(device)) => {
                let mut architecture = D3D12_FEATURE_DATA_ARCHITECTURE::default();
                let asked = unsafe {
                    device.CheckFeatureSupport(
                        D3D12_FEATURE_ARCHITECTURE,
                        &mut architecture as *mut _ as *mut std::ffi::c_void,
                        std::mem::size_of::<D3D12_FEATURE_DATA_ARCHITECTURE>() as u32,
                    )
                };
                if asked.is_ok() {
                    Some(architecture.UMA.as_bool())
                } else {
                    None
                }
            }
            _ => None,
        };

        found.push(GpuAdapter {
            name,
            dedicated: shares_memory == Some(false),
            memory_mb,
            index: position,
        });
    }

    found
}

#[cfg(target_os = "linux")]
fn detect() -> Vec<GpuAdapter> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return found;
    };

    // The "cardN" entries, one per graphics adapter (not the "cardN-HDMI..." outputs).
    let mut cards: Vec<_> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.len() > 4
                && name.starts_with("card")
                && name[4..].chars().all(|c| c.is_ascii_digit())
        })
        .collect();
    cards.sort_by_key(|entry| entry.file_name());

    for (position, entry) in cards.iter().enumerate() {
        let card_dir = entry.path();
        let device_dir = card_dir.join("device");
        let read = |file: &str| {
            std::fs::read_to_string(device_dir.join(file))
                .ok()
                .map(|text| text.trim().to_string())
        };

        let vendor = read("vendor").unwrap_or_default();
        let memory_mb = read("mem_info_vram_total")
            .and_then(|bytes| bytes.parse::<u64>().ok())
            .map(|bytes| bytes / (1024 * 1024))
            .unwrap_or(0);

        // Linux doesn't say "integrated" or "dedicated" directly, so this
        // goes by vendor: NVIDIA is always a separate card; an AMD adapter
        // with plenty of its own memory is a card (APUs report a small
        // share of system memory); Intel is integrated unless it has its own
        // memory (Arc cards).
        let (vendor_name, dedicated) = match vendor.as_str() {
            "0x10de" => ("NVIDIA", true),
            "0x1002" => ("AMD", memory_mb >= 4096),
            "0x8086" => ("Intel", card_dir.join("lmem_total_bytes").exists()),
            _ => ("Unknown", false),
        };
        let kind = if dedicated { "graphics card" } else { "integrated graphics" };

        found.push(GpuAdapter {
            name: format!("{vendor_name} {kind}"),
            dedicated,
            memory_mb,
            index: position as i32,
        });
    }

    found
}

#[cfg(target_os = "macos")]
fn detect() -> Vec<GpuAdapter> {
    // Cutout only supports Apple Silicon Macs, whose graphics are part of the
    // chip. That is integrated graphics, so Auto mode leaves the GPU off.
    vec![GpuAdapter {
        name: "Apple graphics (built into the chip)".to_string(),
        dedicated: false,
        memory_mb: 0,
        index: 0,
    }]
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn detect() -> Vec<GpuAdapter> {
    Vec::new()
}
