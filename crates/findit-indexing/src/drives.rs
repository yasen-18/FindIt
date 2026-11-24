//! Drive discovery.

use crate::win::{wide, wstr_len};
use serde::Serialize;
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveInfo {
    pub letter: char,
    /// For example `C:`.
    pub label: String,
    pub kind: DriveKind,
    /// File system name such as `NTFS` (empty for network drives, which are not
    /// queried here because an offline share can block for a long time).
    pub file_system: String,
    pub volume_name: String,
    pub serial: u32,
}

impl DriveInfo {
    pub fn is_ntfs(&self) -> bool {
        self.file_system.eq_ignore_ascii_case("NTFS")
    }
}

/// Bit mask of present drive letters (bit 0 = A:).
pub fn drive_mask() -> u32 {
    unsafe { GetLogicalDrives() }
}

/// Information about one drive letter, or `None` if it is not present.
pub fn drive_info(letter: char) -> Option<DriveInfo> {
    let letter = letter.to_ascii_uppercase();
    if !letter.is_ascii_uppercase() {
        return None;
    }
    let root = wide(&format!("{letter}:\\"));
    let kind = match unsafe { GetDriveTypeW(root.as_ptr()) } {
        DRIVE_FIXED => DriveKind::Fixed,
        DRIVE_REMOVABLE => DriveKind::Removable,
        DRIVE_REMOTE => DriveKind::Network,
        0 | 1 => return None,
        _ => DriveKind::Other,
    };
    let mut fs = [0u16; 64];
    let mut name = [0u16; 128];
    let mut serial = 0u32;
    if matches!(kind, DriveKind::Fixed | DriveKind::Removable) {
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 && kind == DriveKind::Removable {
            // Card reader without a card, empty DVD drive and similar.
            return None;
        }
    }
    Some(DriveInfo {
        letter,
        label: format!("{letter}:"),
        kind,
        file_system: String::from_utf16_lossy(&fs[..wstr_len(&fs)]),
        volume_name: String::from_utf16_lossy(&name[..wstr_len(&name)]),
        serial,
    })
}

/// All present drives, in letter order.
pub fn list_drives() -> Vec<DriveInfo> {
    let mask = drive_mask();
    (0..26u8).filter(|i| mask & (1 << i) != 0).filter_map(|i| drive_info(char::from(b'A' + i))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_drive_is_listed() {
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let letter = sys.chars().next().unwrap();
        let d = drive_info(letter).expect("system drive");
        assert_eq!(d.kind, DriveKind::Fixed);
        assert!(list_drives().iter().any(|x| x.letter == letter));
    }
}
