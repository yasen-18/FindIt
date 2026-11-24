//! Windows shell actions: open, open with, show in folder, properties, copy to the
//! clipboard, move to the Recycle Bin, file type names and icons.
//!
//! The shell APIs are COM-based and want a single-threaded apartment, so every call
//! runs on one dedicated STA thread that also pumps window messages. Files are
//! always passed to the shell as a path argument, never inside a command string.

use findit_indexing::win::{last_error, long_path, wide, with_nul};
use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use windows_sys::Win32::Foundation::{GlobalFree, HWND, MAX_PATH};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::Storage::FileSystem::{GetShortPathNameW, MoveFileExW};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SHFileOperationW, SHGetFileInfoW, SHOpenFolderAndSelectItems, ShellExecuteExW, FOF_ALLOWUNDO,
    FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING, FO_DELETE, SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    SHFILEINFOW, SHFILEOPSTRUCTW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, DispatchMessageW, GetIconInfo, PeekMessageW, TranslateMessage, HICON, ICONINFO, MSG, PM_REMOVE,
};

const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const SW_SHOWNORMAL: i32 = 1;

type Job = Box<dyn FnOnce() + Send>;

/// Handle to the shell thread.
pub struct Shell {
    tx: Mutex<mpsc::Sender<Job>>,
}

impl Shell {
    pub fn start() -> Shell {
        let (tx, rx) = mpsc::channel::<Job>();
        let _ = std::thread::Builder::new().name("findit-shell".into()).spawn(move || {
            unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
            loop {
                match rx.recv_timeout(Duration::from_millis(30)) {
                    Ok(job) => job(),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                pump();
            }
        });
        Shell { tx: Mutex::new(tx) }
    }

    /// Runs `f` on the shell thread and waits for the result.
    pub fn call<R: Send + 'static>(&self, f: impl FnOnce() -> R + Send + 'static) -> Result<R, String> {
        let (rtx, rrx) = mpsc::sync_channel(1);
        self.tx
            .lock()
            .send(Box::new(move || {
                let _ = rtx.send(f());
            }))
            .map_err(|_| "the shell thread stopped".to_string())?;
        rrx.recv().map_err(|_| "the shell call failed".to_string())
    }
}

fn pump() {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn os_error() -> String {
    std::io::Error::from_raw_os_error(last_error() as i32).to_string()
}

/// A path the shell understands: normal for short paths; for paths over 260
/// characters the 8.3 short form when available, else the `\\?\` form.
pub fn shell_path(path: &[u16]) -> Vec<u16> {
    if path.len() < MAX_PATH as usize {
        return with_nul(path);
    }
    let long = long_path(path);
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetShortPathNameW(long.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n > 0 && n < buf.len() {
        let short = &buf[..n];
        let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
        let short = short.strip_prefix(prefix.as_slice()).unwrap_or(short);
        if short.len() < MAX_PATH as usize {
            return with_nul(short);
        }
    }
    long
}

fn parent_of(path: &[u16]) -> Vec<u16> {
    match path.iter().rposition(|&c| c == u16::from(b'\\')) {
        Some(i) if i > 2 => path[..i].to_vec(),
        Some(i) => path[..=i].to_vec(),
        None => path.to_vec(),
    }
}

/// Opens a file with its default app (`verb` None), or runs a verb such as
/// `openas` (Open with) or `properties`.
pub fn execute(path: &[u16], verb: Option<&str>, owner: HWND) -> Result<(), String> {
    let file = shell_path(path);
    let dir = shell_path(&parent_of(path));
    let verb_w = verb.map(wide);
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    if verb == Some("properties") {
        sei.fMask |= SEE_MASK_INVOKEIDLIST;
    }
    sei.hwnd = owner;
    sei.lpVerb = verb_w.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
    sei.lpFile = file.as_ptr();
    sei.lpDirectory = dir.as_ptr();
    sei.nShow = SW_SHOWNORMAL;
    if unsafe { ShellExecuteExW(&mut sei) } == 0 {
        return Err(os_error());
    }
    Ok(())
}

/// Opens Explorer at the folder containing `path` with the item selected.
pub fn reveal(path: &[u16], owner: HWND) -> Result<(), String> {
    let p = shell_path(path);
    let pidl = unsafe { ILCreateFromPathW(p.as_ptr()) };
    if pidl.is_null() {
        return execute(&parent_of(path), None, owner);
    }
    let hr = unsafe { SHOpenFolderAndSelectItems(pidl, 0, std::ptr::null(), 0) };
    unsafe { ILFree(pidl) };
    if hr < 0 {
        return Err(format!("Explorer could not show the item (0x{:08X})", hr as u32));
    }
    Ok(())
}

/// Moves files to the Recycle Bin. Windows warns before anything would be deleted
/// permanently (for example on drives without a Recycle Bin). Returns true when
/// the user cancelled.
pub fn recycle(paths: &[Vec<u16>], owner: HWND) -> Result<bool, String> {
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        let sp = shell_path(p);
        list.extend_from_slice(&sp[..sp.len() - 1]);
        list.push(0);
    }
    list.push(0);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.hwnd = owner;
    op.wFunc = FO_DELETE;
    op.pFrom = list.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING) as u16;
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 {
        return Err(format!("Windows could not move the item to the Recycle Bin (error 0x{r:X})"));
    }
    Ok(op.fAnyOperationsAborted != 0)
}

/// Renames without ever replacing an existing file.
pub fn rename(old: &[u16], new: &[u16]) -> Result<(), String> {
    let a = long_path(old);
    let b = long_path(new);
    if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 0) } == 0 {
        return Err(os_error());
    }
    Ok(())
}

fn set_clipboard(owner: HWND, items: &[(u32, Vec<u8>)]) -> Result<(), String> {
    let mut opened = false;
    for _ in 0..10 {
        if unsafe { OpenClipboard(owner) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    if !opened {
        return Err("The clipboard is in use by another program".into());
    }
    unsafe { EmptyClipboard() };
    let mut result = Ok(());
    for (fmt, data) in items {
        let h = unsafe { GlobalAlloc(GMEM_MOVEABLE, data.len().max(1)) };
        if h.is_null() {
            result = Err("Out of memory".into());
            break;
        }
        let p = unsafe { GlobalLock(h) } as *mut u8;
        if p.is_null() {
            unsafe { GlobalFree(h) };
            result = Err("Out of memory".into());
            break;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            GlobalUnlock(h);
        }
        if unsafe { SetClipboardData(*fmt, h) }.is_null() {
            unsafe { GlobalFree(h) };
            result = Err(os_error());
            break;
        }
    }
    unsafe { CloseClipboard() };
    result
}

/// Puts text on the clipboard.
pub fn copy_text(text: &str, owner: HWND) -> Result<(), String> {
    let mut data = Vec::with_capacity(text.len() * 2 + 2);
    for u in text.encode_utf16().chain(std::iter::once(0)) {
        data.extend_from_slice(&u.to_le_bytes());
    }
    set_clipboard(owner, &[(u32::from(CF_UNICODETEXT), data)])
}

/// Puts files on the clipboard so they can be pasted in Explorer.
pub fn copy_files(paths: &[Vec<u16>], owner: HWND) -> Result<(), String> {
    // DROPFILES header: pFiles, pt.x, pt.y, fNC, fWide.
    let mut data = Vec::new();
    data.extend_from_slice(&20u32.to_le_bytes());
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(&1u32.to_le_bytes());
    for p in paths {
        for u in shell_path(p) {
            data.extend_from_slice(&u.to_le_bytes());
        }
    }
    data.extend_from_slice(&[0, 0]);
    let fmt_name = wide("Preferred DropEffect");
    let effect_fmt = unsafe { RegisterClipboardFormatW(fmt_name.as_ptr()) };
    let mut items = vec![(u32::from(CF_HDROP), data)];
    if effect_fmt != 0 {
        items.push((effect_fmt, 1u32.to_le_bytes().to_vec()));
    }
    set_clipboard(owner, &items)
}

fn file_info(key: &str, flags: u32) -> Option<SHFILEINFOW> {
    let (name, attrs) = match key {
        "folder" => ("folder".to_string(), FILE_ATTRIBUTE_DIRECTORY),
        "" | "file" => ("file".to_string(), FILE_ATTRIBUTE_NORMAL),
        ext => (format!("file.{ext}"), FILE_ATTRIBUTE_NORMAL),
    };
    let w = wide(&name);
    let mut sfi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let r =
        unsafe { SHGetFileInfoW(w.as_ptr(), attrs, &mut sfi, std::mem::size_of::<SHFILEINFOW>() as u32, flags | SHGFI_USEFILEATTRIBUTES) };
    (r != 0).then_some(sfi)
}

/// Friendly type name ("PDF Document", "File folder") for an extension key.
pub fn type_name(key: &str) -> String {
    match file_info(key, SHGFI_TYPENAME) {
        Some(sfi) => {
            let n = sfi.szTypeName.iter().position(|&c| c == 0).unwrap_or(sfi.szTypeName.len());
            String::from_utf16_lossy(&sfi.szTypeName[..n])
        }
        None => String::new(),
    }
}

/// Small shell icon for an extension key, as a PNG data URL.
pub fn icon_data_url(key: &str) -> Option<String> {
    let sfi = file_info(key, SHGFI_ICON | SHGFI_SMALLICON)?;
    if sfi.hIcon.is_null() {
        return None;
    }
    let png = hicon_to_png(sfi.hIcon);
    unsafe { DestroyIcon(sfi.hIcon) };
    png.map(|p| format!("data:image/png;base64,{}", base64(&p)))
}

fn hicon_to_png(icon: HICON) -> Option<Vec<u8>> {
    let mut ii: ICONINFO = unsafe { std::mem::zeroed() };
    if unsafe { GetIconInfo(icon, &mut ii) } == 0 {
        return None;
    }
    let result = (|| {
        if ii.hbmColor.is_null() {
            return None;
        }
        let mut bm: BITMAP = unsafe { std::mem::zeroed() };
        if unsafe { GetObjectW(ii.hbmColor, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void) } == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 || w > 256 || h > 256 {
            return None;
        }
        let hdc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        let read = |bmp, buf: &mut Vec<u8>| -> bool {
            let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h;
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            unsafe { GetDIBits(hdc, bmp, 0, h as u32, buf.as_mut_ptr() as *mut c_void, &mut bmi, DIB_RGB_COLORS) != 0 }
        };
        let mut color = vec![0u8; (w * h * 4) as usize];
        let ok = read(ii.hbmColor, &mut color);
        if ok && color.chunks_exact(4).all(|p| p[3] == 0) {
            // Old-style icon without alpha: use the mask.
            let mut mask = vec![0u8; (w * h * 4) as usize];
            if !ii.hbmMask.is_null() && read(ii.hbmMask, &mut mask) {
                for (px, m) in color.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                    px[3] = if m[0] == 0 { 255 } else { 0 };
                }
            }
        }
        unsafe { DeleteDC(hdc) };
        if !ok {
            return None;
        }
        for px in color.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Some(png_encode(w as u32, h as u32, &color))
    })();
    unsafe {
        if !ii.hbmColor.is_null() {
            DeleteObject(ii.hbmColor);
        }
        if !ii.hbmMask.is_null() {
            DeleteObject(ii.hbmMask);
        }
    }
    result
}

/// Minimal PNG encoder (RGBA, stored deflate blocks). Icons are tiny, so
/// compression would not be worth a dependency.
pub fn png_encode(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = w as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * h as usize);
    for row in rgba.chunks_exact(stride).take(h as usize) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |ty: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc = crc32fast::Hasher::new();
        crc.update(ty);
        crc.update(data);
        out.extend_from_slice(ty);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc.finalize().to_be_bytes());
    };
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Checks a new file name: not empty, no characters Windows forbids, no reserved
/// device names, no trailing dot or space.
pub fn validate_name(name: &str) -> Result<(), String> {
    let t = crate::strings::t;
    if name.is_empty() || name == "." || name == ".." {
        return Err(t("errors.nameEmpty"));
    }
    if name.encode_utf16().count() > 255 {
        return Err(t("errors.nameTooLong"));
    }
    if name.chars().any(|c| (c as u32) < 32 || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return Err(t("errors.nameInvalidChars"));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(t("errors.nameTrailing"));
    }
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved {
        return Err(t("errors.nameReserved"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("report.pdf").is_ok());
        assert!(validate_name("отчёт 🎉.txt").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("x?.txt").is_err());
        assert!(validate_name("trailing.").is_err());
        assert!(validate_name("CON").is_err());
        assert!(validate_name("com1.txt").is_err());
        assert!(validate_name("COM0").is_ok());
        assert!(validate_name("console.txt").is_ok());
    }

    #[test]
    fn base64_and_png() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        let png = png_encode(2, 2, &[255; 16]);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    #[test]
    fn short_paths_stay_unchanged() {
        let p: Vec<u16> = "C:\\Windows\\notepad.exe".encode_utf16().collect();
        assert_eq!(shell_path(&p), with_nul(&p));
        assert_eq!(String::from_utf16_lossy(&parent_of(&p)), "C:\\Windows");
        let root: Vec<u16> = "C:\\x.txt".encode_utf16().collect();
        assert_eq!(String::from_utf16_lossy(&parent_of(&root)), "C:\\");
    }

    #[test]
    fn shell_info_works() {
        let shell = Shell::start();
        let name = shell.call(|| type_name("folder")).unwrap();
        assert!(!name.is_empty());
        let icon = shell.call(|| icon_data_url("txt")).unwrap();
        assert!(icon.is_some_and(|u| u.starts_with("data:image/png;base64,")));
    }
}
