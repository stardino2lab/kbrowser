//! OS-specific pieces: state dir, Chrome discovery, daemon detach, process
//! cleanup, randomness. Hand-declared FFI keeps the crate dependency-free.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// State directory: `$KB_HOME`, else the per-user default.
pub fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("KB_HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(h);
    }
    default_home()
}

#[cfg(unix)]
fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Chrome executable: `$KB_CHROME`, else well-known locations.
pub fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KB_CHROME").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    chrome_candidates().into_iter().find(|p| p.is_file())
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::process::CommandExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> isize;
        fn SetInformationJobObject(job: isize, class: u32, info: *const c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: isize, process: isize) -> i32;
        fn GetCurrentProcess() -> isize;
        fn GetStdHandle(which: u32) -> isize;
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
    }
    #[link(name = "advapi32")]
    extern "system" {
        // RtlGenRandom
        fn SystemFunction036(buf: *mut u8, len: u32) -> u8;
    }

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        min_working_set: usize,
        max_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const HANDLE_FLAG_INHERIT: u32 = 1;

    pub fn default_home() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("kbrowser")
    }

    pub fn chrome_candidates() -> Vec<PathBuf> {
        let mut v = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(var) {
                let base = PathBuf::from(base);
                v.push(base.join(r"Google\Chrome\Application\chrome.exe"));
                v.push(base.join(r"Chromium\Application\chrome.exe"));
                v.push(base.join(r"Microsoft\Edge\Application\msedge.exe"));
            }
        }
        v.sort_by_key(|p| p.to_string_lossy().contains("Edge"));
        v
    }

    /// Put this process in a job that kills every member when the last handle
    /// closes. Chrome is spawned later and inherits the job, so it cannot
    /// outlive the daemon even if the daemon is killed.
    pub fn kill_children_on_exit() {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if job == 0 {
                crate::util::log("CreateJobObjectW failed");
                return;
            }
            let mut info = ExtendedLimits::default();
            info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &info as *const _ as *const c_void,
                std::mem::size_of::<ExtendedLimits>() as u32,
            );
            if ok == 0 || AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                crate::util::log("job object setup failed; Chrome may outlive a killed daemon");
            }
            // The handle is intentionally leaked: it must live as long as we do.
        }
    }

    pub fn configure_chrome(_cmd: &mut Command) {}

    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultLocaleName(name: *mut u16, len: i32) -> i32;
    }

    /// The user's locale as Windows names it, e.g. "ko-KR".
    pub fn system_locale() -> String {
        let mut buf = [0u16; 85]; // LOCALE_NAME_MAX_LENGTH
        let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
        String::from_utf16_lossy(&buf[..(n.max(1) as usize - 1)])
    }

    pub fn spawn_chrome(mut cmd: Command) -> std::io::Result<std::process::Child> {
        cmd.spawn()
    }

    pub fn spawn_daemon(exe: &std::path::Path) -> std::io::Result<()> {
        // Our own std handles may be inheritable pipes owned by whoever
        // captured our output; a daemon holding them would hang that reader.
        unsafe {
            for which in [-10i32 as u32, -11i32 as u32, -12i32 as u32] {
                let h = GetStdHandle(which);
                if h != 0 && h != -1 {
                    SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
        let spawn = |flags: u32| {
            Command::new(exe)
                .arg("daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(flags)
                .spawn()
        };
        let base = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        // Prefer leaving the caller's job so the daemon survives its shell.
        spawn(base | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(base)).map(|_| ())
    }

    pub fn random_bytes(n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        let ok = unsafe { SystemFunction036(buf.as_mut_ptr(), n as u32) };
        assert!(ok != 0, "RtlGenRandom failed");
        buf
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleMode(handle: isize, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: isize, mode: u32) -> i32;
        fn GetConsoleCP() -> u32;
        fn SetConsoleCP(cp: u32) -> i32;
        fn GetConsoleOutputCP() -> u32;
        fn SetConsoleOutputCP(cp: u32) -> i32;
    }

    /// Console switched to raw VT input/output; restored on drop.
    pub struct RawMode {
        input: Option<(isize, u32)>,
        output: Option<(isize, u32)>,
        cps: (u32, u32),
    }

    impl RawMode {
        /// Works on a console; with pipes (tests) there is nothing to switch.
        pub fn enter() -> RawMode {
            const ENABLE_PROCESSED_INPUT: u32 = 0x1;
            const ENABLE_LINE_INPUT: u32 = 0x2;
            const ENABLE_ECHO_INPUT: u32 = 0x4;
            const ENABLE_QUICK_EDIT_MODE: u32 = 0x40;
            const ENABLE_EXTENDED_FLAGS: u32 = 0x80;
            const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x200;
            const ENABLE_PROCESSED_OUTPUT: u32 = 0x1;
            const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;
            const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x8;
            unsafe {
                let mut raw = RawMode { input: None, output: None, cps: (GetConsoleCP(), GetConsoleOutputCP()) };
                let (hin, hout) = (GetStdHandle(-10i32 as u32), GetStdHandle(-11i32 as u32));
                let mut mode = 0u32;
                if GetConsoleMode(hin, &mut mode) != 0 {
                    raw.input = Some((hin, mode));
                    let new = (mode & !(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_QUICK_EDIT_MODE))
                        | ENABLE_EXTENDED_FLAGS
                        | ENABLE_VIRTUAL_TERMINAL_INPUT;
                    SetConsoleMode(hin, new);
                    SetConsoleCP(65001);
                }
                if GetConsoleMode(hout, &mut mode) != 0 {
                    raw.output = Some((hout, mode));
                    SetConsoleMode(hout, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN);
                    SetConsoleOutputCP(65001);
                }
                raw
            }
        }
        pub fn is_tty(&self) -> bool {
            self.input.is_some() && self.output.is_some()
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            unsafe {
                if let Some((h, mode)) = self.input {
                    SetConsoleMode(h, mode);
                    SetConsoleCP(self.cps.0);
                }
                if let Some((h, mode)) = self.output {
                    SetConsoleMode(h, mode);
                    SetConsoleOutputCP(self.cps.1);
                }
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::io::Read;
    use std::os::unix::process::CommandExt;

    extern "C" {
        fn setsid() -> i32;
        fn getuid() -> u32;
        #[cfg(target_os = "linux")]
        fn prctl(option: i32, ...) -> i32;
    }

    pub fn default_home() -> PathBuf {
        if let Some(x) = std::env::var_os("XDG_STATE_HOME").filter(|x| !x.is_empty()) {
            return PathBuf::from(x).join("kbrowser");
        }
        match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(".local/state/kbrowser"),
            None => std::env::temp_dir().join("kbrowser"),
        }
    }

    pub fn chrome_candidates() -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ]
        .iter()
        .filter_map(|n| in_path(n))
        .collect();
        v.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
        v.push("/Applications/Chromium.app/Contents/MacOS/Chromium".into());
        v
    }

    pub fn kill_children_on_exit() {}

    pub fn system_locale() -> String {
        String::new()
    }

    pub fn configure_chrome(cmd: &mut Command) {
        if unsafe { getuid() } == 0 || std::env::var_os("KB_NO_SANDBOX").is_some() {
            cmd.arg("--no-sandbox");
        }
        // Chrome dies with the daemon; its children follow the browser process.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                const PR_SET_PDEATHSIG: i32 = 1;
                const SIGKILL: usize = 9;
                prctl(PR_SET_PDEATHSIG, SIGKILL);
                Ok(())
            });
        }
    }

    /// Start Chrome from a thread that lives as long as the daemon.
    /// PR_SET_PDEATHSIG fires when the *thread* that forked the child exits,
    /// so a Chrome started from a short-lived request thread would be killed
    /// the moment that request finished.
    pub fn spawn_chrome(cmd: Command) -> std::io::Result<std::process::Child> {
        use std::sync::mpsc::{channel, Sender};
        use std::sync::{Mutex, OnceLock};
        type Job = (Command, Sender<std::io::Result<std::process::Child>>);
        static SPAWNER: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
        let tx = SPAWNER.get_or_init(|| {
            let (tx, rx) = channel::<Job>();
            std::thread::spawn(move || {
                for (mut cmd, reply) in rx {
                    let _ = reply.send(cmd.spawn());
                }
            });
            Mutex::new(tx)
        });
        let (reply, result) = channel();
        let gone = || std::io::Error::new(std::io::ErrorKind::Other, "chrome spawner thread is gone");
        tx.lock().unwrap_or_else(|e| e.into_inner()).send((cmd, reply)).map_err(|_| gone())?;
        result.recv().map_err(|_| gone())?
    }

    pub fn spawn_daemon(exe: &std::path::Path) -> std::io::Result<()> {
        let mut cmd = Command::new(exe);
        cmd.arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                setsid(); // detach from the caller's session and terminal
                Ok(())
            });
        }
        cmd.spawn().map(|_| ())
    }

    pub fn random_bytes(n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .expect("cannot read /dev/urandom");
        buf
    }

    extern "C" {
        fn isatty(fd: i32) -> i32;
        fn tcgetattr(fd: i32, termios: *mut u64) -> i32;
        fn tcsetattr(fd: i32, actions: i32, termios: *const u64) -> i32;
        fn cfmakeraw(termios: *mut u64);
    }

    /// Terminal switched to raw mode; restored on drop. `struct termios` is
    /// treated as an opaque, generously sized, 8-byte-aligned blob so no
    /// per-OS layout is needed.
    pub struct RawMode {
        saved: Option<[u64; 64]>,
    }

    impl RawMode {
        pub fn enter() -> RawMode {
            unsafe {
                if isatty(0) == 0 || isatty(1) == 0 {
                    return RawMode { saved: None };
                }
                let mut saved = [0u64; 64];
                if tcgetattr(0, saved.as_mut_ptr()) != 0 {
                    return RawMode { saved: None };
                }
                let mut raw = saved;
                cfmakeraw(raw.as_mut_ptr());
                tcsetattr(0, 0, raw.as_ptr());
                RawMode { saved: Some(saved) }
            }
        }
        pub fn is_tty(&self) -> bool {
            self.saved.is_some()
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            if let Some(saved) = self.saved {
                unsafe {
                    tcsetattr(0, 0, saved.as_ptr());
                }
            }
        }
    }
}

/// Locale name for `KB_LANG=auto`: the POSIX variables first (they are also
/// set in Unix-like shells on Windows), then what the OS reports.
pub fn os_locale() -> String {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(v) = std::env::var(var).ok().filter(|v| !v.trim().is_empty()) {
            return v;
        }
    }
    imp::system_locale()
}

pub use imp::{configure_chrome, kill_children_on_exit, random_bytes, spawn_chrome, spawn_daemon, RawMode};
use imp::{chrome_candidates, default_home};
