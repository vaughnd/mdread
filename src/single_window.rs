//! Single-window mode (Windows): with the setting on, or with `--same-window`, a launch
//! with a file hands it to the mdread window that receives files, and exits.
//!
//! The receiving role is a named mutex, taken before the window exists, so that several
//! files opened at once from Explorer go to one window even when none is open yet. The
//! receiving window carries a window property of the same name, by which later launches
//! find it, and takes each file as a WM_COPYDATA message in the window procedure that
//! win.rs already wraps. The message is answered on the UI thread, so a window that has
//! stopped responding refuses it, and the sender opens a window of its own: a file is
//! never lost.
//!
//! The name holds the integrity level and a hash of the user's SID and the exe's path, so
//! an elevated window and a normal one, two users, and a dev build and the installed copy
//! each have a receiver of their own. Before it sends a path, the sender checks that the
//! window's process runs as the same user at the same level, because anything that knows
//! the name can put the property on a window, and a lower-integrity process (a browser
//! sandbox) can be sent messages. The receiver needs no such check: Windows does not let
//! a lower-integrity process send WM_COPYDATA to a higher one, and a process at the same
//! level could as well type into the window.
//!
//! With the setting off and no `--same-window`, a launch reads the setting and nothing
//! more: no mutex, no window search.

// Only the Windows code below and the tests use the launch table and the message format.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};

use crate::app::{Args, WindowChoice};

/// What a launch does in single-window mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Hand the file to the receiving window; with none, open a window that receives.
    Forward,
    /// Open a window of its own, which receives if no other window does.
    Claim,
    /// Open a window of its own that never receives.
    Alone,
}

/// The launch table. `setting` is called only by the launches that depend on it, so
/// stdin, `--screenshot` and `--same-window` launches leave the registry alone.
pub fn launch_mode(args: &Args, setting: impl FnOnce() -> bool) -> Mode {
    // A window reading stdin has no file to share, and a screenshot window is about to exit.
    if args.stdin || args.screenshot.is_some() {
        return Mode::Alone;
    }
    if args.window != Some(WindowChoice::Same) && !setting() {
        return Mode::Alone;
    }
    // A theme flag sets the theme of a new window, so such a launch keeps its window.
    let has_file = !args.files.is_empty() && args.theme.is_none();
    if has_file && args.window != Some(WindowChoice::New) { Mode::Forward } else { Mode::Claim }
}

/// A file handed to the receiving window, with the `#anchor` and `--find` it came with.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub path: PathBuf,
    pub anchor: Option<String>,
    pub find: Option<String>,
}

/// Cap on a message, in UTF-16 units: room for the longest path and two long texts.
const MAX_UNITS: usize = 64 * 1024;

/// What a launch sends: its first file, made absolute here because the receiver has
/// another current folder, and the anchor and `--find` given with it.
/// None for a device, pipe or mailslot name, which such a launch opens in a window of its own.
fn request_for(args: &Args) -> Option<Request> {
    let path = std::path::absolute(args.files.first()?).ok()?;
    if names_a_device(&path) {
        return None;
    }
    let given = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
    Some(Request { path, anchor: given(&args.anchor), find: given(&args.find) })
}

/// Whether a path names a device, a pipe or a mailslot rather than a file. The receiver
/// reads a handed-over file on its UI thread, and reading a pipe with no writer or a
/// serial port can block until it answers, which would freeze a window the user already
/// has open. A path wrongly taken for one only costs a window of its own. Works on the
/// text, so that it reads the same in tests on every OS.
fn names_a_device(path: &Path) -> bool {
    let s = path.to_string_lossy().replace('/', "\\").to_lowercase();
    // `\\.\` reaches devices and pipes. `\\?\` does too, except as `\\?\C:\` or
    // `\\?\UNC\server\share`, which are files and shares written long.
    if s.starts_with(r"\\.\") {
        return true;
    }
    let unc = if let Some(rest) = s.strip_prefix(r"\\?\") {
        match rest.strip_prefix(r"unc\") {
            Some(unc) => Some(unc),
            None if rest.as_bytes().get(1..3) == Some(b":\\") => None,
            None => return true,
        }
    } else {
        s.strip_prefix(r"\\")
    };
    // `\\server\pipe\name` and `\\server\mailslot\name` are another machine's (or this one's).
    if unc.and_then(|u| u.split('\\').nth(1)).is_some_and(|share| share == "pipe" || share == "mailslot") {
        return true;
    }
    // The old DOS device names, in any folder and with any extension, as Windows reads them.
    let name = s.rsplit('\\').next().unwrap_or("");
    let stem = name.split('.').next().unwrap_or("").trim_end_matches([' ', ':']);
    let numbered = |p: &str| stem.strip_prefix(p).is_some_and(|n| matches!(n, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"));
    matches!(stem, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$") || numbered("com") || numbered("lpt")
}

/// `path NUL anchor NUL find` in UTF-16, an empty field meaning none. UTF-16 because a
/// Windows path may hold unpaired surrogates, which UTF-8 cannot carry. The fields come
/// from the command line, which cannot hold a NUL.
pub fn encode(r: &Request) -> Vec<u16> {
    let mut units = path_units(&r.path);
    for field in [&r.anchor, &r.find] {
        units.push(0);
        units.extend(field.as_deref().unwrap_or("").encode_utf16());
    }
    units
}

/// The other end of `encode`. None for anything else: another number of fields, a path
/// that is not absolute, text that is not UTF-16, or a message over the cap.
pub fn decode(units: &[u16]) -> Option<Request> {
    if units.len() > MAX_UNITS {
        return None;
    }
    let mut fields = units.split(|&u| u == 0);
    let (path, anchor, find) = (fields.next()?, fields.next()?, fields.next()?);
    if fields.next().is_some() {
        return None;
    }
    let path = path_from_units(path);
    if !path.is_absolute() {
        return None;
    }
    let text = |f: &[u16]| if f.is_empty() { Ok(None) } else { String::from_utf16(f).map(Some) };
    Some(Request { path, anchor: text(anchor).ok()?, find: text(find).ok()? })
}

#[cfg(windows)]
fn path_units(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().collect()
}

#[cfg(windows)]
fn path_from_units(units: &[u16]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(units).into()
}

// Elsewhere only the tests encode and decode.
#[cfg(not(windows))]
fn path_units(p: &Path) -> Vec<u16> {
    p.to_string_lossy().encode_utf16().collect()
}

#[cfg(not(windows))]
fn path_from_units(units: &[u16]) -> PathBuf {
    String::from_utf16_lossy(units).into()
}

#[cfg(windows)]
pub use imp::{forward_or_claim, has_role, on_copy_data, on_destroy, set_setting, setting, start, stop, take_files};

#[cfg(windows)]
mod imp {
    use super::{decode, encode, launch_mode, request_for, Mode, Request, MAX_UNITS};
    use crate::app::Args;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::{null, null_mut};
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
    use std::time::{Duration, Instant};
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE, HWND, LPARAM, LRESULT};
    use windows_sys::Win32::Security::{GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsValidSid, TokenIntegrityLevel, TokenUser, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows_sys::Win32::System::Registry::{RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, RRF_RT_REG_DWORD};
    use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetPropW, GetWindowThreadProcessId, IsIconic, IsWindow, RemovePropW, SendMessageTimeoutW, SetForegroundWindow, SetPropW, ShowWindowAsync, SMTO_ABORTIFHUNG, SW_RESTORE, WM_COPYDATA,
    };

    /// The registry value that holds the setting, a DWORD that is 1 for on. Not part of
    /// app.ron, because it must be known before the window exists, and because every
    /// window writes its own app.ron on exit and would undo a change made in another.
    const KEY: &str = r"Software\mdread";
    const VALUE: &str = "SingleWindow";

    /// Marks mdread's WM_COPYDATA messages, and their format's version.
    const MAGIC: usize = 0x6d64_0001;
    /// The receiver's answer when it took the file.
    const TOOK: LRESULT = 1;
    /// How long a launch waits for a receiver that is starting (the role is taken, the
    /// window not up yet) or closing (it refuses), before opening its own window.
    const WAIT: Duration = Duration::from_secs(5);
    /// How long the receiver has to answer. It answers between two frames, so this is
    /// only reached by a window that is busy for seconds; one that Windows already counts
    /// as hung refuses at once.
    const SEND_MS: u32 = 5000;
    /// `SECURITY_MANDATORY_MEDIUM_RID`: below it (low integrity, AppContainer) a process
    /// takes no part, as it could not be sent files and its window could not be trusted.
    const MEDIUM: u32 = 0x2000;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// The lock, also after a panic elsewhere: nothing done under these locks can leave
    /// them inconsistent.
    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the setting is on. Missing, another value or another type is off.
    pub fn setting() -> bool {
        let mut data = 0u32;
        let mut size = size_of::<u32>() as u32;
        // SAFETY: both strings are NUL-terminated and live to the end of the statement;
        // RRF_RT_REG_DWORD lets only a DWORD into `data`, whose size is passed.
        let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, wide(KEY).as_ptr(), wide(VALUE).as_ptr(), RRF_RT_REG_DWORD, null_mut(), (&raw mut data).cast(), &mut size) };
        rc == ERROR_SUCCESS && data == 1
    }

    /// Saves the setting, creating the key if need be.
    pub fn set_setting(on: bool) -> std::io::Result<()> {
        let data = u32::from(on);
        // SAFETY: as in `setting`; `data` is the 4 bytes passed.
        let rc = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, wide(KEY).as_ptr(), wide(VALUE).as_ptr(), REG_DWORD, (&raw const data).cast(), size_of::<u32>() as u32) };
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(std::io::Error::from_raw_os_error(rc as i32)) }
    }

    /// The user and integrity level a process runs as.
    #[derive(PartialEq, Eq)]
    struct Identity {
        sid: Vec<u8>,
        level: u32,
    }

    /// This process: who it is and the name of its role. None below medium integrity, or
    /// if the token cannot be read.
    struct Me {
        id: Identity,
        name: String,
    }

    fn me() -> Option<&'static Me> {
        static ME: OnceLock<Option<Me>> = OnceLock::new();
        ME.get_or_init(|| {
            // SAFETY: the pseudo handle needs no closing.
            let id = identity(unsafe { GetCurrentProcess() }).filter(|id| id.level >= MEDIUM)?;
            let exe = std::env::current_exe().ok()?;
            // One name for every way of writing the exe's path (8.3 names, subst drives).
            let exe = dunce::canonicalize(&exe).unwrap_or(exe);
            let hash = fnv1a(fnv1a(0xcbf2_9ce4_8422_2325, exe.to_string_lossy().to_lowercase().as_bytes()), &id.sid);
            let name = format!("mdread-{:x}-{hash:016x}", id.level);
            Some(Me { id, name })
        })
        .as_ref()
    }

    /// 64-bit FNV-1a, because std's hasher may change between Rust versions, and two
    /// builds of mdread must agree on the name.
    fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
        bytes.iter().fold(seed, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
    }

    fn identity(process: HANDLE) -> Option<Identity> {
        let mut token = null_mut();
        // SAFETY: `token` is written on success.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return None;
        }
        // SAFETY: a fresh handle that nothing else owns.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let token = token.as_raw_handle();
        let user = token_info(token, TokenUser)?;
        // SAFETY: the call filled the buffer with a TOKEN_USER, whose SID points into it.
        let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        // SAFETY: `IsValidSid` only reads; a valid SID is `GetLengthSid` bytes long.
        let sid = unsafe { (IsValidSid(sid) != 0).then(|| std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize).to_vec())? };
        let label = token_info(token, TokenIntegrityLevel)?;
        // SAFETY: as for TokenUser. The level is the label SID's last sub-authority, and
        // the index is below the count.
        let level = unsafe {
            let sid = (*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
            if IsValidSid(sid) == 0 {
                return None;
            }
            *GetSidSubAuthority(sid, u32::from((*GetSidSubAuthorityCount(sid)).checked_sub(1)?))
        };
        Some(Identity { sid, level })
    }

    /// One class of token information, in a buffer aligned for the structures it holds.
    fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<Vec<u64>> {
        let mut len = 0;
        // SAFETY: a null buffer only asks for the size.
        unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` has room for `len` bytes.
        (len > 0 && unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) } != 0).then_some(buf)
    }

    /// The role's mutex while this process has it.
    static ROLE: Mutex<Option<OwnedHandle>> = Mutex::new(None);

    /// Takes the receiving role unless another process has it; true if this one has it.
    fn claim(me: &Me) -> bool {
        let mut role = lock(&ROLE);
        if role.is_none() {
            *role = create_role(&format!(r"Local\{}", me.name));
        }
        role.is_some()
    }

    /// The mutex `name`, if this call created it. Not inheritable, so the child that the
    /// renderer fallback starts does not keep it.
    fn create_role(name: &str) -> Option<OwnedHandle> {
        // SAFETY: the name is NUL-terminated; no security attributes means not inheritable.
        let h = unsafe { CreateMutexW(null(), 0, wide(name).as_ptr()) };
        // SAFETY: read straight after the call it reports on.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // Null: the name exists and is not ours to open, so it is taken all the same.
        // SAFETY: a fresh handle that nothing else owns; dropping it closes it.
        (!h.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(h) }).filter(|_| !existed)
    }

    /// Whether this process has the receiving role (taken in `main`, or by `start`).
    pub fn has_role() -> bool {
        lock(&ROLE).is_some()
    }

    /// Before the window: hands this launch's file to the receiving window, or takes the
    /// role so that this launch's window receives. True if the file was handed over and
    /// the launch should exit.
    pub fn forward_or_claim(args: &Args) -> bool {
        let mode = launch_mode(args, setting);
        if mode == Mode::Alone {
            return false;
        }
        let Some(me) = me() else { return false };
        let request = request_for(args).filter(|r| mode == Mode::Forward && encode(r).len() <= MAX_UNITS);
        let Some(request) = request else {
            claim(me);
            return false;
        };
        let t0 = Instant::now();
        loop {
            if claim(me) {
                log::debug!("single-window mode: no window receives, this one will");
                return false;
            }
            if let Some(hwnd) = find_receiver(me) {
                match send(hwnd, &request) {
                    Sent::Took => {
                        bring_to_front(hwnd);
                        log::debug!("single-window mode: handed over in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
                        return true;
                    }
                    // Not receiving any more (it is closing, or the setting was turned off
                    // in it): its role is given up soon.
                    Sent::Refused => {}
                    // Closed after it was found: a closing window gives up the role in
                    // WM_DESTROY, so this one can take it, or another launch has, and its
                    // window is found once it is up. Hung or busy: it keeps the role.
                    Sent::Failed => {
                        if claim(me) {
                            log::debug!("single-window mode: the receiving window closed; this one will receive");
                            return false;
                        }
                        // SAFETY: a plain query; a stale handle gives 0.
                        if unsafe { IsWindow(hwnd) } != 0 {
                            log::info!("single-window mode: the receiving window did not answer; opening a window of its own");
                            return false;
                        }
                    }
                }
            }
            if t0.elapsed() >= WAIT {
                log::info!("single-window mode: no window took the file in time; opening a window of its own");
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The window that receives for this role, if its process is this user at this level.
    fn find_receiver(me: &Me) -> Option<HWND> {
        struct Search<'a> {
            name: Vec<u16>,
            id: &'a Identity,
            found: HWND,
        }
        unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
            // SAFETY: `lparam` is the `Search` below, alive for the whole enumeration.
            let s = unsafe { &mut *(lparam as *mut Search) };
            // SAFETY: reading a window's property is harmless for any window.
            if !unsafe { GetPropW(hwnd, s.name.as_ptr()) }.is_null() && window_identity(hwnd).is_some_and(|id| id == *s.id) {
                s.found = hwnd;
                return 0;
            }
            1
        }
        let mut search = Search { name: wide(&me.name), id: &me.id, found: null_mut() };
        // SAFETY: `visit` only uses `search`, which outlives the call.
        unsafe { EnumWindows(Some(visit), (&raw mut search) as LPARAM) };
        (!search.found.is_null()).then_some(search.found)
    }

    fn window_identity(hwnd: HWND) -> Option<Identity> {
        let mut pid = 0;
        // SAFETY: `pid` is written; 0 if the window has gone.
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        // SAFETY: plain call; null on failure.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if pid == 0 || process.is_null() {
            return None;
        }
        // SAFETY: a fresh handle that nothing else owns.
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        identity(process.as_raw_handle())
    }

    enum Sent {
        Took,
        Refused,
        Failed,
    }

    fn send(hwnd: HWND, request: &Request) -> Sent {
        let units = encode(request);
        let data = COPYDATASTRUCT { dwData: MAGIC, cbData: (units.len() * 2) as u32, lpData: units.as_ptr().cast_mut().cast() };
        let mut answer = 0;
        // SAFETY: `data` and `units` outlive the call; Windows copies them to the receiver.
        let sent = unsafe { SendMessageTimeoutW(hwnd, WM_COPYDATA, 0, (&raw const data) as LPARAM, SMTO_ABORTIFHUNG, SEND_MS, &mut answer) };
        match (sent, answer as LRESULT) {
            (0, _) => Sent::Failed,
            (_, TOOK) => Sent::Took,
            _ => Sent::Refused,
        }
    }

    /// Restores and raises the window that took the file. The sending launch does this
    /// rather than the receiver, because Windows lets only the process the user started
    /// last take the foreground; started from a terminal, it may only flash the taskbar
    /// button instead.
    fn bring_to_front(hwnd: HWND) {
        // SAFETY: plain calls on a window handle; a stale one only makes them fail.
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindowAsync(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
    }

    /// Files handed to this window and not yet opened, while it receives.
    struct Inbox {
        hwnd: isize,
        ctx: egui::Context,
        files: Vec<Request>,
    }

    static INBOX: Mutex<Option<Inbox>> = Mutex::new(None);

    /// Makes window `hwnd` receive, taking the role first if no other process has it; true
    /// if it receives.
    pub fn start(hwnd: isize, ctx: &egui::Context) -> bool {
        let Some(me) = me() else { return false };
        if !claim(me) {
            return false;
        }
        crate::win::wrap_window_procedure(hwnd);
        lock(&INBOX).get_or_insert_with(|| Inbox { hwnd, ctx: ctx.clone(), files: Vec::new() });
        // SAFETY: the name is NUL-terminated; the value only has to be non-null.
        unsafe { SetPropW(hwnd as HWND, wide(&me.name).as_ptr(), 1 as HANDLE) };
        true
    }

    /// Stops receiving: senders no longer find the window, and the next launch can take
    /// the role.
    pub fn stop() {
        let inbox = lock(&INBOX).take();
        if let (Some(inbox), Some(me)) = (inbox, me()) {
            // SAFETY: as in `start`. Also frees the atom Windows made for the name.
            unsafe { RemovePropW(inbox.hwnd as HWND, wide(&me.name).as_ptr()) };
        }
        lock(&ROLE).take();
    }

    /// The files handed over since the last call, in the order they came.
    pub fn take_files() -> Vec<Request> {
        lock(&INBOX).as_mut().map(|inbox| std::mem::take(&mut inbox.files)).unwrap_or_default()
    }

    /// WM_COPYDATA, from the window procedure: keeps the file for the next frame if this
    /// window receives. Must not panic, as it runs inside a Windows callback.
    pub fn on_copy_data(lparam: LPARAM) -> LRESULT {
        // SAFETY: for WM_COPYDATA, `lparam` points to a COPYDATASTRUCT that is valid until
        // the message returns.
        let data = unsafe { &*(lparam as *const COPYDATASTRUCT) };
        let len = data.cbData as usize;
        if data.dwData != MAGIC || data.lpData.is_null() || len > MAX_UNITS * 2 {
            return 0;
        }
        // Read as bytes, as Windows' copy of the data need not be aligned for u16.
        // SAFETY: `lpData` holds `cbData` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(data.lpData.cast::<u8>(), len) };
        let (pairs, odd) = bytes.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|&p| u16::from_le_bytes(p)).collect();
        let mut inbox = lock(&INBOX);
        let (Some(inbox), Some(request), true) = (inbox.as_mut(), decode(&units), odd.is_empty()) else { return 0 };
        inbox.files.push(request);
        inbox.ctx.request_repaint();
        TOOK
    }

    /// WM_DESTROY: a receiving window that closes gives up the role at once, so the next
    /// launch need not wait for this process to end.
    pub fn on_destroy(hwnd: isize) {
        if lock(&INBOX).as_ref().is_some_and(|inbox| inbox.hwnd == hwnd) {
            stop();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn only_one_holder_of_a_role_at_a_time() {
            let name = format!(r"Local\mdread-test-{}", std::process::id());
            let first = create_role(&name);
            assert!(first.is_some());
            assert!(create_role(&name).is_none(), "taken while held");
            drop(first);
            assert!(create_role(&name).is_some(), "free once closed");
        }

        #[test]
        fn this_process_takes_part_at_medium_integrity() {
            let me = me().expect("tests run at medium integrity or above");
            assert!(me.name.starts_with("mdread-"));
            assert!(identity(unsafe { GetCurrentProcess() }) == Some(Identity { sid: me.id.sid.clone(), level: me.id.level }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ThemeChoice;

    fn args(files: &[&str]) -> Args {
        Args { files: files.iter().map(PathBuf::from).collect(), ..Args::default() }
    }

    #[test]
    fn launch_table() {
        let on = || true;
        let off = || false;
        let unread = || -> bool { panic!("the setting should not be read") };
        let with = |mut a: Args, f: fn(&mut Args)| {
            f(&mut a);
            a
        };
        assert_eq!(launch_mode(&args(&["a.md"]), off), Mode::Alone);
        assert_eq!(launch_mode(&args(&["a.md"]), on), Mode::Forward);
        assert_eq!(launch_mode(&args(&[]), on), Mode::Claim);
        assert_eq!(launch_mode(&args(&[]), off), Mode::Alone);
        let new = |a: &mut Args| a.window = Some(WindowChoice::New);
        assert_eq!(launch_mode(&with(args(&["a.md"]), new), on), Mode::Claim);
        assert_eq!(launch_mode(&with(args(&["a.md"]), new), off), Mode::Alone);
        let same = |a: &mut Args| a.window = Some(WindowChoice::Same);
        assert_eq!(launch_mode(&with(args(&["a.md"]), same), unread), Mode::Forward);
        assert_eq!(launch_mode(&with(args(&[]), same), unread), Mode::Claim);
        let dark = |a: &mut Args| a.theme = Some(ThemeChoice::Dark);
        assert_eq!(launch_mode(&with(args(&["a.md"]), dark), on), Mode::Claim);
        let stdin = |a: &mut Args| {
            a.stdin = true;
            a.window = Some(WindowChoice::Same);
        };
        assert_eq!(launch_mode(&with(args(&[]), stdin), unread), Mode::Alone);
        let shot = |a: &mut Args| a.screenshot = Some("x.png".into());
        assert_eq!(launch_mode(&with(args(&["a.md"]), shot), unread), Mode::Alone);
    }

    #[test]
    fn a_request_survives_the_round_trip() {
        let path = std::env::temp_dir().join("notes").join("a b.md");
        for (anchor, find) in [(None, None), (Some("x-y"), Some("naïve 日本")), (None, Some("q"))] {
            let r = Request { path: path.clone(), anchor: anchor.map(Into::into), find: find.map(Into::into) };
            assert_eq!(decode(&encode(&r)), Some(r));
        }
    }

    #[test]
    fn decode_refuses_anything_else() {
        let abs = std::env::temp_dir().join("a.md");
        let units = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        let msg = |p: &Path, rest: &str| [path_units(p), units(rest)].concat();
        assert!(decode(&msg(&abs, "\0\0")).is_some());
        assert_eq!(decode(&msg(Path::new("a.md"), "\0\0")), None, "relative path");
        assert_eq!(decode(&msg(&abs, "\0")), None, "two fields");
        assert_eq!(decode(&msg(&abs, "\0\0\0")), None, "four fields");
        assert_eq!(decode(&[]), None);
        let mut bad = msg(&abs, "\0");
        bad.push(0xd800); // an unpaired surrogate in the anchor
        bad.push(0);
        assert_eq!(decode(&bad), None, "anchor not UTF-16");
        let mut long = msg(&abs, "\0\0");
        long.resize(MAX_UNITS + 1, u16::from(b'x'));
        assert_eq!(decode(&long), None, "over the cap");
    }

    #[test]
    fn the_request_holds_an_absolute_path() {
        let r = request_for(&Args { files: vec!["a.md".into()], anchor: Some("x".into()), find: Some(String::new()), ..Args::default() }).unwrap();
        assert!(r.path.is_absolute() && r.path.ends_with("a.md"));
        assert_eq!((r.anchor.as_deref(), r.find), (Some("x"), None));
        assert_eq!(request_for(&args(&[])), None);
    }

    #[test]
    fn devices_pipes_and_mailslots_are_not_handed_over() {
        for p in [
            r"\\.\pipe\x",
            r"\\.\COM1",
            r"\\.\C:\notes\a.md",
            r"\\?\pipe\x",
            r"\\?\GLOBALROOT\Device\Null",
            r"\\server\pipe\x",
            r"\\localhost\MailSlot\x",
            r"\\?\UNC\server\pipe\x",
            r"C:\notes\CON",
            r"C:\notes\nul.md",
            r"C:\notes\com1 .md",
            r"C:\notes\LPT9",
            r"C:\notes\COM¹.md",
            r"C:\notes\CONIN$",
            r"COM1:",
            "//./pipe/x",
        ] {
            assert!(names_a_device(Path::new(p)), "{p}");
        }
        for p in [
            r"C:\notes\a.md",
            r"\\?\C:\notes\a.md",
            r"\\server\share\a.md",
            r"\\?\UNC\server\share\a.md",
            r"\\server\share\pipe\a.md",
            r"C:\notes\pipe\a.md",
            r"C:\notes\console.md",
            r"C:\notes\com10.md",
            r"C:\notes\com.md",
            r"C:\notes\aux-notes.md",
            "/home/a/notes.md",
        ] {
            assert!(!names_a_device(Path::new(p)), "{p}");
        }
    }
}
