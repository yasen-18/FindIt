//! The elevated helper process and its named pipe.
//!
//! Security model (spec section 8):
//! * The app starts the helper with the `runas` verb, so Windows shows a UAC prompt.
//!   Declining it is reported as [`LaunchError::Declined`] and the app keeps working
//!   in standard mode.
//! * The helper creates the pipe with `FILE_FLAG_FIRST_PIPE_INSTANCE` (no squatting),
//!   `PIPE_REJECT_REMOTE_CLIENTS`, and a DACL that only allows the current user's SID.
//!   A medium integrity label lets the non-elevated app connect.
//! * Both ends verify each other's process id: the helper only accepts its parent,
//!   the app only talks to the process it launched.
//! * The helper drops every privilege except backup (read) and exits when the app exits.

use crate::win::{last_error, wide, OwnedHandle};
use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    LocalFree, ERROR_BROKEN_PIPE, ERROR_CANCELLED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HLOCAL, HWND, LUID,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeValueW, TokenElevation, TokenPrivileges, TokenUser, LUID_AND_ATTRIBUTES,
    PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SE_PRIVILEGE_ENABLED, SE_PRIVILEGE_REMOVED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessId, OpenProcess, OpenProcessToken, TerminateProcess, WaitForSingleObject, INFINITE,
    PROCESS_SYNCHRONIZE,
};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const STILL_ACTIVE: u32 = 259;
const SW_HIDE: i32 = 0;

/// A random pipe name such as `FindIt-3f9a...` (128 bits from the system RNG).
pub fn random_pipe_name() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    let status = unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), 16, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status != 0 {
        return Err(io::Error::other(format!("BCryptGenRandom failed: {status:#x}")));
    }
    Ok(format!("FindIt-{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// True for names produced by [`random_pipe_name`]. The helper refuses anything else.
pub fn is_valid_pipe_name(name: &str) -> bool {
    name.strip_prefix("FindIt-").is_some_and(|h| h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn pipe_path(name: &str) -> Vec<u16> {
    wide(&format!("\\\\.\\pipe\\{name}"))
}

fn token(access: u32) -> io::Result<OwnedHandle> {
    let mut h = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut h) } == 0 {
        return Err(io::Error::last_os_error());
    }
    OwnedHandle::new(h).ok_or_else(io::Error::last_os_error)
}

fn token_info(tok: &OwnedHandle, class: i32) -> io::Result<Vec<u64>> {
    let mut len = 0u32;
    unsafe { GetTokenInformation(tok.raw(), class, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    if unsafe { GetTokenInformation(tok.raw(), class, buf.as_mut_ptr() as *mut c_void, len, &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

/// The current user's SID, for example `S-1-5-21-...`.
pub fn current_user_sid() -> io::Result<String> {
    let tok = token(TOKEN_QUERY)?;
    let buf = token_info(&tok, TokenUser)?;
    let tu = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let mut s: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(tu.User.Sid, &mut s) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0usize;
    while unsafe { *s.add(len) } != 0 {
        len += 1;
    }
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, len) });
    unsafe { LocalFree(s as HLOCAL) };
    Ok(out)
}

/// True when this process runs elevated (as administrator).
pub fn is_elevated() -> bool {
    let Ok(tok) = token(TOKEN_QUERY) else { return false };
    token_info(&tok, TokenElevation).is_ok_and(|b| unsafe { (*(b.as_ptr() as *const TOKEN_ELEVATION)).TokenIsElevated } != 0)
}

fn privilege_luid(name: &str) -> Option<LUID> {
    let w = wide(name);
    let mut luid = LUID { LowPart: 0, HighPart: 0 };
    (unsafe { LookupPrivilegeValueW(std::ptr::null(), w.as_ptr(), &mut luid) } != 0).then_some(luid)
}

/// Removes every privilege from this process except backup (read any folder) and
/// change-notify, so the elevated helper cannot write, delete or debug anything.
pub fn drop_privileges_except_backup() -> io::Result<()> {
    let tok = token(TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES)?;
    let mut buf = token_info(&tok, TokenPrivileges)?;
    let backup = privilege_luid("SeBackupPrivilege");
    let notify = privilege_luid("SeChangeNotifyPrivilege");
    let same = |a: &LUID, b: &Option<LUID>| b.is_some_and(|b| a.LowPart == b.LowPart && a.HighPart == b.HighPart);
    let tp = buf.as_mut_ptr() as *mut TOKEN_PRIVILEGES;
    let count = unsafe { (*tp).PrivilegeCount } as usize;
    let first = unsafe { std::ptr::addr_of_mut!((*tp).Privileges) as *mut LUID_AND_ATTRIBUTES };
    for i in 0..count {
        let p = unsafe { &mut *first.add(i) };
        p.Attributes = if same(&p.Luid, &backup) || same(&p.Luid, &notify) { SE_PRIVILEGE_ENABLED } else { SE_PRIVILEGE_REMOVED };
    }
    if unsafe { AdjustTokenPrivileges(tok.raw(), 0, tp, 0, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Exits this process as soon as process `pid` ends (the helper follows the app).
pub fn exit_with_parent(pid: u32) -> io::Result<()> {
    let h = OwnedHandle::new(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) }).ok_or_else(io::Error::last_os_error)?;
    std::thread::Builder::new().name("findit-parent-watch".into()).spawn(move || {
        unsafe { WaitForSingleObject(h.raw(), INFINITE) };
        std::process::exit(0);
    })?;
    Ok(())
}

/// A connected pipe end.
pub struct PipeStream(OwnedHandle);

impl std::fmt::Debug for PipeStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PipeStream")
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0u32;
        let len = buf.len().min(u32::MAX as usize) as u32;
        if unsafe { ReadFile(self.0.raw(), buf.as_mut_ptr(), len, &mut n, std::ptr::null_mut()) } == 0 {
            let e = last_error();
            if e == ERROR_BROKEN_PIPE {
                return Ok(0);
            }
            return Err(io::Error::from_raw_os_error(e as i32));
        }
        Ok(n as usize)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut n = 0u32;
        let len = buf.len().min(1 << 30) as u32;
        if unsafe { WriteFile(self.0.raw(), buf.as_ptr(), len, &mut n, std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Helper side: creates the locked-down pipe and waits for the app (process
/// `expected_client`) to connect.
pub fn serve(name: &str, expected_client: u32) -> io::Result<PipeStream> {
    if !is_valid_pipe_name(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid pipe name"));
    }
    let sid = current_user_sid()?;
    // Only this user (GA = generic all); medium integrity so the non-elevated app may connect.
    let sddl = wide(&format!("D:P(A;;GA;;;{sid})S:(ML;;NW;;;ME)"));
    let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut psd, std::ptr::null_mut()) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sa =
        SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd, bInheritHandle: 0 };
    let path = pipe_path(name);
    let h = unsafe {
        CreateNamedPipeW(
            path.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            1 << 20,
            1 << 16,
            0,
            &sa,
        )
    };
    unsafe { LocalFree(psd as HLOCAL) };
    let pipe = OwnedHandle::new(h).ok_or_else(io::Error::last_os_error)?;
    if unsafe { ConnectNamedPipe(pipe.raw(), std::ptr::null_mut()) } == 0 && last_error() != ERROR_PIPE_CONNECTED {
        return Err(io::Error::last_os_error());
    }
    let mut client = 0u32;
    if unsafe { GetNamedPipeClientProcessId(pipe.raw(), &mut client) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if client != expected_client {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unexpected pipe client"));
    }
    Ok(PipeStream(pipe))
}

/// A helper process started by [`launch_elevated`].
pub struct ElevatedChild {
    handle: OwnedHandle,
    pub pid: u32,
}

impl std::fmt::Debug for ElevatedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElevatedChild").field("pid", &self.pid).finish()
    }
}

impl ElevatedChild {
    pub fn is_running(&self) -> bool {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.handle.raw(), &mut code) != 0 && code == STILL_ACTIVE }
    }
    pub fn terminate(&self) {
        if self.is_running() {
            unsafe { TerminateProcess(self.handle.raw(), 0) };
        }
    }
}

#[derive(Debug)]
pub enum LaunchError {
    /// The user said no to the UAC prompt.
    Declined,
    Failed(io::Error),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::Declined => f.write_str("the administrator prompt was declined"),
            LaunchError::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// Starts `exe` with administrator rights (UAC prompt). `owner` is the window the
/// prompt belongs to (may be null).
pub fn launch_elevated(exe: &Path, args: &str, owner: HWND) -> Result<ElevatedChild, LaunchError> {
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(args);
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    sei.hwnd = owner;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpParameters = params.as_ptr();
    sei.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut sei) } == 0 {
        let e = last_error();
        return Err(if e == ERROR_CANCELLED { LaunchError::Declined } else { LaunchError::Failed(io::Error::from_raw_os_error(e as i32)) });
    }
    let handle = OwnedHandle::new(sei.hProcess).ok_or_else(|| LaunchError::Failed(io::Error::other("no process handle")))?;
    let pid = unsafe { GetProcessId(handle.raw()) };
    Ok(ElevatedChild { handle, pid })
}

/// App side: connects to the helper's pipe and checks that the server really is
/// the helper we launched.
pub fn connect(name: &str, child: &ElevatedChild, timeout: Duration) -> io::Result<PipeStream> {
    let path = pipe_path(name);
    let deadline = Instant::now() + timeout;
    loop {
        let h = unsafe {
            CreateFileW(path.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
        };
        if let Some(pipe) = OwnedHandle::new(h) {
            let mut server = 0u32;
            if unsafe { GetNamedPipeServerProcessId(pipe.raw(), &mut server) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if server != child.pid {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "pipe server is not the FindIt helper"));
            }
            return Ok(PipeStream(pipe));
        }
        let e = last_error();
        if !child.is_running() {
            return Err(io::Error::other("the helper exited before connecting"));
        }
        if Instant::now() > deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for the helper"));
        }
        if e == ERROR_PIPE_BUSY {
            unsafe { WaitNamedPipeW(path.as_ptr(), 500) };
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_names() {
        let n = random_pipe_name().unwrap();
        assert!(is_valid_pipe_name(&n), "{n}");
        assert_ne!(n, random_pipe_name().unwrap());
        assert!(!is_valid_pipe_name("FindIt-../../x"));
        assert!(!is_valid_pipe_name("Other-00000000000000000000000000000000"));
    }

    #[test]
    fn sid_and_elevation() {
        assert!(current_user_sid().unwrap().starts_with("S-1-"));
        let _ = is_elevated();
    }

    #[test]
    fn pipe_accepts_only_the_expected_client() {
        let name = random_pipe_name().unwrap();
        let me = std::process::id();
        let n2 = name.clone();
        let server = std::thread::spawn(move || serve(&n2, me));
        // Connect as a plain client (we are the expected pid).
        let path = pipe_path(&name);
        let deadline = Instant::now() + Duration::from_secs(5);
        let client = loop {
            let h = unsafe {
                CreateFileW(path.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
            };
            if let Some(p) = OwnedHandle::new(h) {
                break PipeStream(p);
            }
            assert!(Instant::now() < deadline, "could not connect");
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut srv = server.join().unwrap().unwrap();
        let mut cli = client;
        crate::proto::write_msg(&mut srv, &crate::proto::HelperMsg::Hello { version: 1 }).unwrap();
        assert_eq!(crate::proto::read_msg(&mut cli).unwrap(), crate::proto::HelperMsg::Hello { version: 1 });
        // A second server with the same name is refused (first instance only).
        assert!(serve(&name, me).is_err());
        // A client with the wrong pid is rejected.
        let other = random_pipe_name().unwrap();
        let o2 = other.clone();
        let server = std::thread::spawn(move || serve(&o2, me.wrapping_add(1)));
        let path = pipe_path(&other);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let h = unsafe {
                CreateFileW(path.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
            };
            if OwnedHandle::new(h).is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "could not connect");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(server.join().unwrap().unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
