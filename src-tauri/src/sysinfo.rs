//! Small facts about the system: Windows build, accent color, memory use.

use findit_indexing::win::wide;
use std::ffi::c_void;
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

fn reg_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = [0u16; 128];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size)
    };
    if r != 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

fn reg_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let (k, v) = (wide(key), wide(value));
    let mut out = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut out as *mut u32 as *mut c_void, &mut size)
    };
    (r == 0).then_some(out)
}

/// Windows build number (22000 and later is Windows 11).
pub fn windows_build() -> u32 {
    reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "CurrentBuildNumber")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The user's accent color as `#rrggbb`.
pub fn accent_color() -> Option<String> {
    // Stored as 0xAABBGGRR.
    let v = reg_dword(HKEY_CURRENT_USER, r"Software\Microsoft\Windows\DWM", "AccentColor")?;
    Some(format!("#{:02x}{:02x}{:02x}", v & 0xFF, (v >> 8) & 0xFF, (v >> 16) & 0xFF))
}

/// Private memory of this process in MB.
pub fn memory_mb() -> f64 {
    let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) } == 0 {
        return 0.0;
    }
    pmc.PagefileUsage as f64 / (1024.0 * 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts() {
        assert!(windows_build() >= 10_000);
        assert!(memory_mb() > 0.0);
        if let Some(c) = accent_color() {
            assert_eq!(c.len(), 7);
        }
    }
}
