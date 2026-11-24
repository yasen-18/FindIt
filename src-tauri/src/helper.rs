//! The elevated fast-mode helper (same executable, different entry point) and the
//! crash dialog.
//!
//! The helper is started by the app with `--findit-helper --pipe <name> --parent
//! <pid> --volumes C,D:<journal>:<usn>`. Every argument is validated; anything
//! unexpected makes it exit without doing anything.

use findit_indexing::fast::{run_fast, FastRequest};
use findit_indexing::pipe;
use findit_indexing::proto::{write_msg, HelperMsg, PROTOCOL_VERSION};
use findit_indexing::win::wide;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONERROR, MB_SETFOREGROUND, MB_YESNO};

const FLAG: &str = "--findit-helper";

pub fn is_helper_invocation() -> bool {
    std::env::args().nth(1).as_deref() == Some(FLAG)
}

struct HelperArgs {
    pipe: String,
    parent: u32,
    volumes: Vec<FastRequest>,
}

fn parse_args(args: &[String]) -> Option<HelperArgs> {
    if args.first().map(String::as_str) != Some(FLAG) || args.len() != 7 {
        return None;
    }
    let mut pipe_name = None;
    let mut parent = None;
    let mut volumes = None;
    for pair in args[1..].chunks(2) {
        match (pair[0].as_str(), pair[1].as_str()) {
            ("--pipe", v) if pipe::is_valid_pipe_name(v) => pipe_name = Some(v.to_string()),
            ("--parent", v) => parent = v.parse::<u32>().ok().filter(|&p| p != 0),
            ("--volumes", v) => {
                let list: Option<Vec<FastRequest>> = v.split(',').map(FastRequest::parse).collect();
                volumes = list.filter(|l| !l.is_empty() && l.len() <= 26);
            }
            _ => return None,
        }
    }
    Some(HelperArgs { pipe: pipe_name?, parent: parent?, volumes: volumes? })
}

/// Entry point of the helper process. Returns the exit code.
pub fn helper_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(a) = parse_args(&args) else { return 2 };
    if pipe::exit_with_parent(a.parent).is_err() {
        return 3;
    }
    // Give up if the app never connects.
    let connected = Arc::new(AtomicBool::new(false));
    let c2 = connected.clone();
    let _ = std::thread::Builder::new().name("findit-helper-timeout".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(30));
        if !c2.load(Ordering::Relaxed) {
            std::process::exit(4);
        }
    });
    let mut stream = match pipe::serve(&a.pipe, a.parent) {
        Ok(s) => s,
        Err(_) => return 5,
    };
    connected.store(true, Ordering::Relaxed);
    if write_msg(&mut stream, &HelperMsg::Hello { version: PROTOCOL_VERSION }).is_err() {
        return 6;
    }
    let stop = AtomicBool::new(false);
    let mut send = |m: HelperMsg| write_msg(&mut stream, &m).is_ok();
    run_fast(&a.volumes, &mut send, &stop, &mut || {
        // Volume handles are open; from here on only read access is needed.
        let _ = pipe::drop_privileges_except_backup();
    });
    0
}

/// Logs unexpected errors and offers to restart (spec section 7).
pub fn install_crash_handler() {
    static SHOWN: AtomicBool = AtomicBool::new(false);
    std::panic::set_hook(Box::new(|info| {
        log::error!(target: "findit::crash", "unexpected error: {info}");
        log::logger().flush();
        if SHOWN.swap(true, Ordering::SeqCst) {
            return;
        }
        let title = wide(&crate::strings::t("crash.title"));
        let body = wide(&crate::strings::t("crash.body"));
        let answer =
            unsafe { MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONERROR | MB_SETFOREGROUND) };
        if answer == IDYES {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe).arg("--restart-after").arg(std::process::id().to_string()).spawn();
            }
        }
        std::process::exit(1);
    }));
}

/// After a crash restart, waits for the old process to be gone so the
/// single-instance check does not hand control back to it.
pub fn wait_for_previous_instance() {
    let args: Vec<String> = std::env::args().collect();
    let Some(pos) = args.iter().position(|a| a == "--restart-after") else { return };
    let Some(pid) = args.get(pos + 1).and_then(|p| p.parse::<u32>().ok()) else { return };
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if let Some(h) = findit_indexing::win::OwnedHandle::new(h) {
        unsafe { WaitForSingleObject(h.raw(), 10_000) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn helper_arguments_are_strict() {
        let name = pipe::random_pipe_name().unwrap();
        let ok = parse_args(&s(&[FLAG, "--pipe", &name, "--parent", "1234", "--volumes", "C,D:5:77"])).unwrap();
        assert_eq!(ok.parent, 1234);
        assert_eq!(ok.volumes.len(), 2);
        assert!(parse_args(&s(&[FLAG, "--pipe", "evil", "--parent", "1", "--volumes", "C"])).is_none());
        assert!(parse_args(&s(&[FLAG, "--pipe", &name, "--parent", "0", "--volumes", "C"])).is_none());
        assert!(parse_args(&s(&[FLAG, "--pipe", &name, "--parent", "1", "--volumes", "C;rm"])).is_none());
        assert!(parse_args(&s(&[FLAG, "--pipe", &name, "--parent", "1", "--volumes", "C", "--extra", "x"])).is_none());
        assert!(parse_args(&s(&[FLAG, "--pipe", &name, "--run", "1", "--volumes", "C"])).is_none());
    }
}
