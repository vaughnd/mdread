//! Single-instance mode (Windows only): which launch claims the receiving role, forwards its files to the window that already has it, or stands alone with a window of its own. The decision is `launch_mode`, a pure function of the arguments and the setting. A forwarded launch travels as a `Request` (`encode_request` / `decode_request`) over a named pipe whose name is `pipe_name`; the pipe itself (server, sender, identity checks, stop) is in `pipe.rs`.

use crate::app::{file_stamp, markdown_in_folder, Args, FileStamp, WindowChoice};
use crate::{links, text};
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, RRF_RT_REG_DWORD};

mod pipe;
pub use pipe::{create_pipe, forward, name_for_this_exe, own_identity, shutdown, CreateError, ForwardOutcome, Identity, PipeHandle, Server};
/// The pipe tests' serialising guards, test names and test server, for the app's tests of the setting.
#[cfg(test)]
pub(crate) use pipe::tests as pipe_tests;

/// What a launch does about the single-instance pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchMode {
    /// Try to forward this launch to the receiving window; if none receives, create the pipe and receive.
    ForwardOrClaim,
    /// Open a window of its own; receive if no window does.
    Claim,
    /// Open a window of its own with no pipe.
    Standalone,
}

/// The launch table: what each combination of file, flags and setting does. Pure: the setting is a closure so it is read only by the launches whose row says so
/// (not for `--same-window`, `--screenshot` or stdin, which skip the registry read on the startup path).
pub fn launch_mode(args: &Args, read_setting: impl FnOnce() -> bool) -> LaunchMode {
    // Stdin and --screenshot win over everything, --same-window included: their own window, never a pipe.
    if args.stdin || args.screenshot.is_some() {
        return LaunchMode::Standalone;
    }
    let has_file = !args.files.is_empty();
    // A theme flag belongs to one window (a session theme), so such a launch never forwards, with or without a file.
    let forwards = has_file && args.theme.is_none();
    if args.window == Some(WindowChoice::SameWindow) {
        // Takes part whatever the setting says, so the setting is not read.
        return if forwards { LaunchMode::ForwardOrClaim } else { LaunchMode::Claim };
    }
    if !read_setting() {
        return LaunchMode::Standalone;
    }
    // Setting on: --new-window opens its own window (claiming only if none receives); a plain file launch forwards.
    if forwards && args.window.is_none() { LaunchMode::ForwardOrClaim } else { LaunchMode::Claim }
}

/// What a forwarding launch asks the receiving window to do: open `path`, jump to `anchor`, start a search for `find`.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub path: PathBuf,
    pub anchor: Option<String>,
    pub find: Option<String>,
}

/// Why `decode_request` refused a message. The receiver answers all of them the same way (`refused`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Odd number of bytes: not UTF-16.
    Odd,
    /// More than `MAX_MESSAGE` bytes.
    TooLong,
    /// The first field is not `mdread/1` (also an empty message).
    Version,
    /// The second field is not `open`.
    Command,
    /// Not exactly five fields.
    Fields,
    /// The path is empty or not absolute (`\x`, `C:x` and `x\y` count as relative).
    RelativePath,
    /// The anchor is not text or has more than `MAX_FIELD_CHARS` characters.
    Anchor,
    /// The find text is not text or has more than `MAX_FIELD_CHARS` characters.
    Find,
}

const VERSION: &str = "mdread/1";
const COMMAND_OPEN: &str = "open";
/// Cap on a whole request (room for a long path plus both text fields); the pipe layer sizes its read buffer with it.
pub const MAX_MESSAGE: usize = 96 * 1024;
/// Cap on the anchor and on the find text, in characters.
const MAX_FIELD_CHARS: usize = 4096;

/// `mdread/1 \0 open \0 <path> \0 <anchor> \0 <find>` as UTF-16LE bytes; an empty field means none. UTF-16 because a
/// Windows path can hold unpaired surrogates, which UTF-8 cannot carry. The text fields cannot hold a NUL (the
/// command line cannot), and the path cannot either; one that did would only make the receiver refuse the message.
pub fn encode_request(r: &Request) -> Vec<u8> {
    let mut units: Vec<u16> = VERSION.encode_utf16().collect();
    units.push(0);
    units.extend(COMMAND_OPEN.encode_utf16());
    units.push(0);
    units.extend(r.path.as_os_str().encode_wide());
    units.push(0);
    units.extend(r.anchor.as_deref().unwrap_or("").encode_utf16());
    units.push(0);
    units.extend(r.find.as_deref().unwrap_or("").encode_utf16());
    units.into_iter().flat_map(u16::to_le_bytes).collect()
}

/// Parses bytes received from another process. Release builds abort on panic, so nothing here may panic whatever the
/// bytes are: no indexing, no `unwrap`, no unchecked slicing.
pub fn decode_request(bytes: &[u8]) -> Result<Request, DecodeError> {
    if bytes.len() > MAX_MESSAGE {
        return Err(DecodeError::TooLong);
    }
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(DecodeError::Odd);
    }
    let units: Vec<u16> = pairs.iter().map(|&p| u16::from_le_bytes(p)).collect();
    let mut fields = units.split(|&u| u == 0);
    let mut next = || fields.next().ok_or(DecodeError::Fields);
    if !is_ascii_field(next()?, VERSION) {
        return Err(DecodeError::Version);
    }
    if !is_ascii_field(next()?, COMMAND_OPEN) {
        return Err(DecodeError::Command);
    }
    let path = PathBuf::from(OsString::from_wide(next()?));
    let anchor = next()?;
    let find = next()?;
    if fields.next().is_some() {
        return Err(DecodeError::Fields);
    }
    if !path.is_absolute() {
        return Err(DecodeError::RelativePath);
    }
    Ok(Request { path, anchor: text_field(anchor, DecodeError::Anchor)?, find: text_field(find, DecodeError::Find)? })
}

fn is_ascii_field(field: &[u16], expected: &str) -> bool {
    field.iter().copied().eq(expected.encode_utf16())
}

/// An anchor or find field: empty is none; otherwise valid UTF-16 of at most `MAX_FIELD_CHARS` characters.
fn text_field(field: &[u16], err: DecodeError) -> Result<Option<String>, DecodeError> {
    if field.is_empty() {
        return Ok(None);
    }
    let s = String::from_utf16(field).map_err(|_| err)?;
    if s.chars().count() > MAX_FIELD_CHARS {
        return Err(err);
    }
    Ok(Some(s))
}

/// 64-bit FNV-1a, written out because std's `DefaultHasher` is not stable across Rust versions and two mdread
/// builds must agree on the pipe name.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// The part of the pipe name that tells executables apart: FNV-1a of the ASCII-lower-cased canonical path (the raw
/// path when it cannot be canonicalized), as 16 lower-case hex digits. Canonicalizing removes 8.3 names, `subst`
/// drives and junctions, so every way of naming one exe gives one key, while a dev build and the installed copy differ.
/// `dunce` keeps the `\\?\` prefix off, so the key does not depend on which form the OS hands back.
pub fn exe_key(exe: &Path) -> String {
    let canonical = dunce::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let lower = canonical.to_string_lossy().to_ascii_lowercase();
    format!("{:016x}", fnv1a64(lower.as_bytes()))
}

/// `\\.\pipe\mdread-<user SID>-<logon id>-<integrity RID>-<exe key>`. The SID is the usual `S-1-5-...` string; the
/// logon id (`TokenStatistics.AuthenticationId`) and the integrity RID (for example 0x3000, medium plus) are written
/// as lower-case hex without leading zeros, so a name reads like the values Windows tools show.
pub fn pipe_name(sid: &str, logon: u64, level: u32, key: &str) -> String {
    format!(r"\\.\pipe\mdread-{sid}-{logon:x}-{level:x}-{key}")
}

/// An absolute form of `p` that does not touch the file system (the sender's current folder is lost once the request
/// crosses the pipe); `p` itself if that is impossible.
pub fn make_absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// What `claim_or_forward` made of this launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// This launch holds the pipe (in `PENDING` until `App::new` serves it) and opens the receiving window.
    Receiver,
    /// The receiving window took the file: this launch exits without a window.
    Forwarded,
    /// A window of its own and no pipe (the setting is off, the role is taken, or something failed: fail open).
    Standalone,
}

/// How long a sender tries at most before it opens a window of its own, counted from process start: it covers waiting for a busy pipe, for `ready`
/// from a receiver that is still starting, and the retries as receiver.
const SENDER_DEADLINE: Duration = Duration::from_secs(10);
/// Pause before claiming again after a refusal (the receiver is closing), so the loop does not hammer it while its
/// server stops.
const RETRY_PAUSE: Duration = Duration::from_millis(20);
/// How long refusals in a row are retried. A closing receiver frees the name within its 200 ms stop wait, so this
/// covers it; a receiver that keeps refusing (it hangs, or it has no worker free) does not make the sender wait for
/// the whole deadline: the sender opens a window of its own with the file instead.
const REFUSED_RETRY: Duration = Duration::from_millis(500);

/// The receiver's pipe between `main` and `App::new`, which takes it with `take_pending` and serves it. Process-wide
/// so `main` keeps no handle in `Args`; `shutdown` reaches the handle through the pipe registry, not through this.
static PENDING: Mutex<Option<PipeHandle>> = Mutex::new(None);

/// The pipe this launch claimed, once. Poison-tolerant: nothing under the lock can panic.
pub fn take_pending() -> Option<PipeHandle> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Claims the pipe or forwards the file for this process: the real pipe name and identity, and a deadline 10 s after `args.launched` (taken
/// first thing in `main`). Low integrity and AppContainer launches, and any failure to find the name, stand alone.
pub fn claim_or_forward(args: &Args, mode: LaunchMode) -> Claim {
    if mode == LaunchMode::Standalone {
        return Claim::Standalone;
    }
    let started = Instant::now();
    let deadline = args.launched.unwrap_or(started) + SENDER_DEADLINE;
    let (id, name) = match own_identity().and_then(|id| Ok((id, name_for_this_exe()?))) {
        Ok(found) => found,
        Err(e) => {
            log::info!("single-window mode: not taking part ({e})");
            return Claim::Standalone;
        }
    };
    let claim = claim_or_forward_named(&name, &id, args, mode, deadline);
    log::debug!("single-window mode: {mode:?} -> {claim:?} in {} us", started.elapsed().as_micros());
    claim
}

/// What a forwarding launch sends: its first file made absolute against the current folder (the receiver has another
/// one), with the anchor and `--find`. An empty anchor or find is none, as on the wire.
fn request_for(args: &Args) -> Option<Request> {
    let non_empty = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
    let file = args.files.first()?;
    Some(Request { path: make_absolute(file), anchor: non_empty(&args.anchor), find: non_empty(&args.find) })
}

/// `claim_or_forward` on a given name, identity and deadline, so tests can run it on their own pipe names.
fn claim_or_forward_named(name: &str, id: &Identity, args: &Args, mode: LaunchMode, deadline: Instant) -> Claim {
    let req = match mode {
        LaunchMode::Standalone => return Claim::Standalone,
        LaunchMode::Claim => None,
        // `launch_mode` only forwards a launch with a file; without one there is nothing to send, so only claim.
        LaunchMode::ForwardOrClaim => request_for(args),
    };
    // A path that is still not absolute (`mdread ""`) is refused by every receiver's decoder, so forwarding it could
    // only be refused again and again: open a window of its own at once, which shows the error, and leave the pipe
    // alone.
    if req.as_ref().is_some_and(|r| !r.path.is_absolute()) {
        return Claim::Standalone;
    }
    // When the current run of refusals started; any other outcome ends the run.
    let mut refused_since: Option<Instant> = None;
    loop {
        match create_pipe(name, id) {
            Ok(pipe) => {
                *PENDING.lock().unwrap_or_else(PoisonError::into_inner) = Some(pipe);
                return Claim::Receiver;
            }
            Err(CreateError::Taken) => {}
            Err(CreateError::Other(e)) => {
                log::warn!("single-window pipe {name}: cannot create it ({e}); opening a window of its own");
                return Claim::Standalone;
            }
        }
        // `Claim` with the role taken: another window receives, so this one opens a window of its own.
        let Some(req) = &req else { return Claim::Standalone };
        if Instant::now() >= deadline {
            log::warn!("single-window pipe {name}: no receiver answered in time; opening a window of its own");
            return Claim::Standalone;
        }
        match forward(name, id, req, deadline) {
            ForwardOutcome::Delivered => return Claim::Forwarded,
            // The receiver went away between the two calls: claim again.
            ForwardOutcome::NoReceiver => refused_since = None,
            // The receiver is closing, hangs, has no worker free, or died before `commit`; the client handle is
            // closed already. Claim again for a short while (a closing receiver frees the name soon), then stand alone.
            ForwardOutcome::Refused => {
                let since = *refused_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= REFUSED_RETRY {
                    log::info!("single-window pipe {name}: the receiver keeps refusing; opening a window of its own");
                    return Claim::Standalone;
                }
                std::thread::sleep(RETRY_PAUSE);
            }
            ForwardOutcome::Failed => return Claim::Standalone,
        }
    }
}

/// A forwarded file as the worker read it: the canonical path, its text and its stamp from before the read.
pub struct Opened {
    pub path: PathBuf,
    pub decoded: text::Decoded,
    pub stamp: Option<FileStamp>,
}

/// What the forward worker hands to the UI thread.
pub struct ForwardResult {
    /// The open ticket taken when the request was committed; `App::apply_forward` drops the result only when the user
    /// navigated after that (other forwards do not count).
    pub gen: u64,
    pub request: Request,
    /// The document, or the status text that says why not.
    pub outcome: Result<Opened, String>,
    /// When `commit` ran on the pipe thread, for the "shown N ms after the request arrived" log line.
    pub arrived: Instant,
}

/// Reads a forwarded file, on a worker thread (never on the pipe thread or the UI thread: a share can take tens of seconds):
/// the path filter on the received path before anything touches it, a shortcut's target (filtered again), a folder's
/// README, then the canonical path, filtered once more, its stamp, the read and `text::decode`. The second filter
/// guards the read only: `is_dir` and `canonicalize` have already followed any symbolic link or junction in the
/// path to wherever it leads (the same exposure as `mdread <path>` on the command line), but a link that resolves
/// to a device, a pipe or another refused form is never read. The original `Path` is opened throughout, never a
/// lossy string. Errors are the status texts of a drop and of the command line. Must not panic: release builds abort.
pub fn resolve_forward(req: &Request) -> Result<Opened, String> {
    let refused = |p: &Path| format!("Unsupported path: {}", links::display_path(p));
    if !links::is_openable_local_path(&req.path) {
        return Err(refused(&req.path));
    }
    let mut path = req.path.clone();
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
        let _com = crate::win::ComApartment::init();
        match crate::win::shortcut_target(&path) {
            Some(target) if links::is_openable_local_path(&target) => path = target,
            Some(target) => return Err(refused(&target)),
            None => return Err(format!("{} does not point to a file", links::display_path(&path))),
        }
    }
    if path.is_dir() {
        path = markdown_in_folder(&path).ok_or_else(|| format!("No Markdown files in {}", links::display_path(&path)))?;
    }
    let cannot_read = |p: &Path, e: io::Error| format!("Cannot read {}: {e}", links::display_path(p));
    // No fallback to the path as given, unlike `load_path_in`: a path that cannot be canonicalized cannot be filtered
    // before the read.
    let canonical = dunce::canonicalize(&path).map_err(|e| cannot_read(&path, e))?;
    if !links::is_openable_local_path(&canonical) {
        return Err(refused(&canonical));
    }
    let stamp = file_stamp(&canonical);
    let bytes = std::fs::read(&canonical).map_err(|e| cannot_read(&canonical, e))?;
    Ok(Opened { path: canonical, decoded: text::decode(bytes), stamp })
}

/// The registry key and value that hold the setting: `HKCU\Software\mdread` `SingleWindow`, a REG_DWORD,
/// 1 = on. It is a registry value, not a field of `Settings`, because eframe rewrites `app.ron` from every window's own
/// copy and would put a setting turned on in one window back to off at the next save of another.
const SETTING_KEY: &str = r"Software\mdread";
const SETTING_VALUE: &str = "SingleWindow";

/// NUL-terminated UTF-16 for the registry calls.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Whether single-window mode is on. One `RegGetValueW` with no key handle kept, run on every launch that needs the
/// setting, so it must stay this small (startup time comes first); its duration goes to the log at `debug` level.
pub fn read_setting() -> bool {
    let started = std::time::Instant::now();
    let on = read_setting_at(SETTING_KEY);
    log::debug!("single-window setting read in {} us: {on}", started.elapsed().as_micros());
    on
}

/// On when `HKCU\<subkey>` `SingleWindow` is a REG_DWORD equal to 1. A missing key or value, another value, another
/// type (a string, a QWORD) and any error all read as off, so nothing a user or another program writes there can turn
/// the mode on by accident. `RRF_RT_REG_DWORD` makes the call itself refuse the other types.
pub(crate) fn read_setting_at(subkey: &str) -> bool {
    let sub = wide(subkey);
    let name = wide(SETTING_VALUE);
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `sub` and `name` are NUL-terminated; `data` is writable for the `size` bytes passed; the type pointer may be null.
    let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, sub.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, null_mut(), (&raw mut data).cast(), &mut size) };
    rc == ERROR_SUCCESS && data == 1
}

/// Writes the setting at once, when the checkbox changes (never from `app.ron`, see `SETTING_KEY`). Creates the key if it is missing.
pub fn write_setting(on: bool) -> io::Result<()> {
    write_setting_at(SETTING_KEY, on)
}

pub(crate) fn write_setting_at(subkey: &str, on: bool) -> io::Result<()> {
    let sub = wide(subkey);
    let name = wide(SETTING_VALUE);
    let data = u32::from(on);
    // SAFETY: `sub` and `name` are NUL-terminated; `data` is valid for the 4 bytes passed.
    let rc = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, sub.as_ptr(), name.as_ptr(), REG_DWORD, (&raw const data).cast(), size_of::<u32>() as u32) };
    if rc == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(rc as i32)) }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::ThemeChoice;
    use std::cell::Cell;
    use std::path::PathBuf;
    use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegDeleteTreeW, REG_QWORD, REG_SZ};

    /// Runs `launch_mode` with a counting closure; returns the mode and how often the setting was read.
    fn run(args: &Args, setting: bool) -> (LaunchMode, u32) {
        let calls = Cell::new(0);
        let mode = launch_mode(args, || {
            calls.set(calls.get() + 1);
            setting
        });
        (mode, calls.get())
    }

    fn file(window: Option<WindowChoice>) -> Args {
        Args { files: vec![PathBuf::from("a.md")], window, ..Args::default() }
    }

    fn no_file(window: Option<WindowChoice>) -> Args {
        Args { window, ..Args::default() }
    }

    fn theme(files: bool, window: Option<WindowChoice>) -> Args {
        let mut a = if files { file(window) } else { no_file(window) };
        a.theme = Some(ThemeChoice::Dark);
        a
    }

    fn stdin(window: Option<WindowChoice>) -> Args {
        Args { stdin: true, window, ..Args::default() }
    }

    fn screenshot(window: Option<WindowChoice>) -> Args {
        Args { screenshot: Some(PathBuf::from("out.png")), files: vec![PathBuf::from("a.md")], window, ..Args::default() }
    }

    use LaunchMode::*;
    use WindowChoice::*;

    #[test]
    fn launch_mode_file_no_flag() {
        assert_eq!(run(&file(None), true), (ForwardOrClaim, 1));
        assert_eq!(run(&file(None), false), (Standalone, 1));
    }

    #[test]
    fn launch_mode_file_new_window() {
        assert_eq!(run(&file(Some(NewWindow)), true), (Claim, 1));
        assert_eq!(run(&file(Some(NewWindow)), false), (Standalone, 1));
    }

    #[test]
    fn launch_mode_file_same_window() {
        assert_eq!(run(&file(Some(SameWindow)), true), (ForwardOrClaim, 0));
        assert_eq!(run(&file(Some(SameWindow)), false), (ForwardOrClaim, 0));
    }

    #[test]
    fn launch_mode_no_file_no_flag() {
        assert_eq!(run(&no_file(None), true), (Claim, 1));
        assert_eq!(run(&no_file(None), false), (Standalone, 1));
    }

    #[test]
    fn launch_mode_no_file_new_window() {
        assert_eq!(run(&no_file(Some(NewWindow)), true), (Claim, 1));
        assert_eq!(run(&no_file(Some(NewWindow)), false), (Standalone, 1));
    }

    #[test]
    fn launch_mode_no_file_same_window() {
        assert_eq!(run(&no_file(Some(SameWindow)), true), (Claim, 0));
        assert_eq!(run(&no_file(Some(SameWindow)), false), (Claim, 0));
    }

    #[test]
    fn launch_mode_theme_no_flag() {
        for files in [false, true] {
            assert_eq!(run(&theme(files, None), true), (Claim, 1));
            assert_eq!(run(&theme(files, None), false), (Standalone, 1));
        }
    }

    #[test]
    fn launch_mode_theme_new_window() {
        for files in [false, true] {
            assert_eq!(run(&theme(files, Some(NewWindow)), true), (Claim, 1));
            assert_eq!(run(&theme(files, Some(NewWindow)), false), (Standalone, 1));
        }
    }

    #[test]
    fn launch_mode_theme_same_window_claims_without_reading() {
        for files in [false, true] {
            assert_eq!(run(&theme(files, Some(SameWindow)), true), (Claim, 0));
            assert_eq!(run(&theme(files, Some(SameWindow)), false), (Claim, 0));
        }
    }

    #[test]
    fn launch_mode_stdin_never_claims_and_never_reads() {
        for window in [None, Some(NewWindow), Some(SameWindow)] {
            for setting in [true, false] {
                assert_eq!(run(&stdin(window), setting), (Standalone, 0));
            }
        }
    }

    #[test]
    fn launch_mode_screenshot_never_claims_and_never_reads() {
        for window in [None, Some(NewWindow), Some(SameWindow)] {
            for setting in [true, false] {
                assert_eq!(run(&screenshot(window), setting), (Standalone, 0));
            }
        }
    }

    #[test]
    fn launch_mode_same_window_with_screenshot_or_stdin_is_standalone() {
        assert_eq!(run(&screenshot(Some(SameWindow)), false), (Standalone, 0));
        assert_eq!(run(&stdin(Some(SameWindow)), false), (Standalone, 0));
        // Stdin together with a file or a theme flag still stands alone.
        let mut a = file(Some(SameWindow));
        a.stdin = true;
        a.theme = Some(ThemeChoice::Light);
        assert_eq!(run(&a, true), (Standalone, 0));
    }

    // ---- protocol, exe key, pipe name ----

    fn req(path: impl Into<PathBuf>, anchor: Option<&str>, find: Option<&str>) -> Request {
        Request { path: path.into(), anchor: anchor.map(String::from), find: find.map(String::from) }
    }

    fn encode_str(fields: &[&str]) -> Vec<u8> {
        fields.join("\0").encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn request_round_trips() {
        let surrogate = PathBuf::from(OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800, 0x2e, 0x6d, 0x64]));
        let long = PathBuf::from(format!(r"\\?\C:\{}\notes.md", "d".repeat(290)));
        assert!(long.as_os_str().len() >= 300);
        let cases = [
            req(r"C:\x y\notes #1.md", None, None),
            req(r"C:\x y\notes #1.md", Some("intro"), Some("a b")),
            req("C:\\日本\\é.md", Some("見出し"), None),
            req(surrogate, None, Some("x")),
            req(long, Some("intro"), Some("a b")),
        ];
        for r in cases {
            let bytes = encode_request(&r);
            assert_eq!(decode_request(&bytes), Ok(r.clone()), "{:?}", r.path);
        }
        // The surrogate really survives as an unpaired unit, not as U+FFFD.
        let r = req(OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800]), None, None);
        let back = decode_request(&encode_request(&r)).unwrap();
        assert_eq!(back.path.as_os_str().encode_wide().last(), Some(0xd800));
    }

    #[test]
    fn request_wire_format() {
        let r = req(r"C:\a.md", Some("top"), None);
        assert_eq!(encode_request(&r), encode_str(&["mdread/1", "open", r"C:\a.md", "top", ""]));
    }

    #[test]
    fn decode_rejects() {
        let good = ["mdread/1", "open", r"C:\a.md", "", ""];
        assert!(decode_request(&encode_str(&good)).is_ok());
        let with = |i: usize, v: &str| {
            let mut f = good;
            f[i] = v;
            encode_str(&f)
        };
        assert_eq!(decode_request(&with(0, "mdread/2")), Err(DecodeError::Version));
        assert_eq!(decode_request(&with(1, "close")), Err(DecodeError::Command));
        assert_eq!(decode_request(&with(2, r"notes\a.md")), Err(DecodeError::RelativePath));
        assert_eq!(decode_request(&with(2, "")), Err(DecodeError::RelativePath));
        assert_eq!(decode_request(&with(2, r"\a.md")), Err(DecodeError::RelativePath));
        assert_eq!(decode_request(&with(2, "C:a.md")), Err(DecodeError::RelativePath));
        assert_eq!(decode_request(&with(3, &"a".repeat(4097))), Err(DecodeError::Anchor));
        assert_eq!(decode_request(&with(4, &"a".repeat(4097))), Err(DecodeError::Find));
        assert!(decode_request(&with(3, &"a".repeat(4096))).is_ok());
        assert!(decode_request(&with(4, &"é".repeat(4096))).is_ok());
        // Counted in characters, not UTF-16 units: 4 096 astral characters (8 192 units) pass.
        assert!(decode_request(&with(4, &"\u{1F600}".repeat(4096))).is_ok());
        assert_eq!(decode_request(&with(4, &"\u{1F600}".repeat(4097))), Err(DecodeError::Find));
        // Wrong field counts.
        assert_eq!(decode_request(&encode_str(&["mdread/1", "open", r"C:\a.md", ""])), Err(DecodeError::Fields));
        assert_eq!(decode_request(&encode_str(&["mdread/1", "open", r"C:\a.md", "", "", ""])), Err(DecodeError::Fields));
        assert_eq!(decode_request(&encode_str(&["mdread/1"])), Err(DecodeError::Fields));
        assert_eq!(decode_request(&[]), Err(DecodeError::Version));
        // An unpaired surrogate in anchor or find is not text.
        let mut units: Vec<u16> = "mdread/1\0open\0C:\\a.md\0".encode_utf16().collect();
        units.extend([0xd800, 0]);
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(decode_request(&bytes), Err(DecodeError::Anchor));
        units.splice(units.len() - 2.., [0, 0xdc00]);
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(decode_request(&bytes), Err(DecodeError::Find));
        // Odd byte count, and one byte over the cap (which is also odd: too long wins).
        let mut odd = encode_str(&good);
        odd.push(0);
        assert_eq!(decode_request(&odd), Err(DecodeError::Odd));
        assert_eq!(decode_request(&vec![0u8; MAX_MESSAGE + 1]), Err(DecodeError::TooLong));
        assert_eq!(decode_request(&vec![0u8; MAX_MESSAGE + 2]), Err(DecodeError::TooLong));
        assert_eq!(MAX_MESSAGE, 96 * 1024);
        // Exactly at the cap it is parsed (and fails on content, not length).
        assert_eq!(decode_request(&vec![0u8; MAX_MESSAGE]), Err(DecodeError::Version));
    }

    #[test]
    fn decode_never_panics() {
        let valid = encode_request(&req(r"C:\x y\日本.md", Some("intro"), Some("a b")));
        for n in 0..=valid.len() {
            let _ = decode_request(&valid[..n]);
        }
        for n in 0..valid.len() {
            let _ = decode_request(&valid[n..]);
        }
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..5000 {
            let len = (next() % 600) as usize;
            let mut buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Half of the buffers start like a real message, with some bytes damaged, so the later fields get exercised.
            if next() % 2 == 0 {
                for (slot, b) in buf.iter_mut().zip(valid.iter()) {
                    if next() % 8 != 0 {
                        *slot = *b;
                    }
                }
            }
            let _ = decode_request(&buf);
        }
    }

    #[test]
    fn fnv1a_known_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn exe_key_is_stable_and_case_insensitive() {
        let a = exe_key(Path::new(r"C:\A\mdread.exe"));
        assert_eq!(a, exe_key(Path::new(r"c:\a\MDREAD.EXE")));
        assert_eq!(a, format!("{:016x}", fnv1a64(br"c:\a\mdread.exe")));
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_ne!(a, exe_key(Path::new(r"C:\B\mdread.exe")));
    }

    #[test]
    fn exe_key_of_short_and_long_names_match() {
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let dir = std::env::temp_dir().join(format!("mdread-exekey-{}", std::process::id()));
        let long_dir = dir.join("a long directory name");
        std::fs::create_dir_all(&long_dir).unwrap();
        let exe = long_dir.join("a long exe name.exe");
        std::fs::write(&exe, b"x").unwrap();
        let wide: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
        let mut buf = [0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `buf` is writable for the length passed.
        let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let short = PathBuf::from(OsString::from_wide(buf.get(..n).unwrap_or(&[])));
        let key_long = exe_key(&exe);
        let key_short = exe_key(&short);
        let differs = n != 0 && !short.to_string_lossy().eq_ignore_ascii_case(&exe.to_string_lossy());
        std::fs::remove_dir_all(&dir).unwrap();
        if !differs {
            // 8.3 names are switched off on this volume, so there is no short form to compare.
            eprintln!("exe_key short-name check skipped: no 8.3 name for {}", exe.display());
            return;
        }
        assert_eq!(key_long, key_short, "{} vs {}", exe.display(), short.display());
    }

    #[test]
    fn pipe_name_format() {
        assert_eq!(
            pipe_name("S-1-5-21-1-2-3-1001", 0x1a2b3, 0x3000, "0123456789abcdef"),
            r"\\.\pipe\mdread-S-1-5-21-1-2-3-1001-1a2b3-3000-0123456789abcdef"
        );
    }

    #[test]
    fn make_absolute_keeps_absolute_and_resolves_relative() {
        assert_eq!(make_absolute(Path::new(r"C:\a\b.md")), PathBuf::from(r"C:\a\b.md"));
        assert!(make_absolute(Path::new(r"notes\b.md")).is_absolute());
    }

    /// A private test key under `HKCU\Software` (never the real `Software\mdread`), unique per process and test, that is
    /// deleted with everything in it when the guard drops, also when the test fails.
    pub(crate) struct TestKey(pub(crate) String);

    impl TestKey {
        pub(crate) fn new(tag: &str) -> TestKey {
            let key = TestKey(format!(r"Software\mdread-test-{}-{tag}", std::process::id()));
            assert_ne!(key.0, SETTING_KEY, "the tests must not touch the real key");
            key
        }

        /// Sets `SingleWindow` to raw data of the given registry type, creating the key.
        fn set_raw(&self, ty: u32, data: &[u8]) {
            let sub = wide(&self.0);
            let name = wide(SETTING_VALUE);
            // SAFETY: both strings are NUL-terminated and `data` is valid for the length passed.
            let rc = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, sub.as_ptr(), name.as_ptr(), ty, data.as_ptr().cast(), data.len() as u32) };
            assert_eq!(rc, 0, "RegSetKeyValueW failed");
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            let sub = wide(&self.0);
            // SAFETY: `sub` is NUL-terminated. A key that was never created gives an error, which is fine here.
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, sub.as_ptr()) };
        }
    }

    #[test]
    fn setting_missing_key_and_missing_value_are_off() {
        let key = TestKey::new("missing");
        assert!(!read_setting_at(&key.0), "key does not exist");
        key.set_raw(REG_DWORD, &1u32.to_le_bytes());
        assert!(read_setting_at(&key.0));
        // The key exists now; a different value name must not count, and neither does a deleted value.
        let sub = wide(&key.0);
        let name = wide(SETTING_VALUE);
        // SAFETY: both strings are NUL-terminated.
        assert_eq!(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, sub.as_ptr(), name.as_ptr()) }, 0);
        assert!(!read_setting_at(&key.0), "key exists, value does not");
    }

    #[test]
    fn setting_write_true_then_false() {
        let key = TestKey::new("write");
        write_setting_at(&key.0, true).unwrap();
        assert!(read_setting_at(&key.0));
        write_setting_at(&key.0, false).unwrap();
        assert!(!read_setting_at(&key.0));
        write_setting_at(&key.0, true).unwrap();
        assert!(read_setting_at(&key.0));
    }

    #[test]
    fn setting_dword_written_by_hand_is_one_only_for_one() {
        let key = TestKey::new("dword");
        key.set_raw(REG_DWORD, &1u32.to_le_bytes());
        assert!(read_setting_at(&key.0));
        key.set_raw(REG_DWORD, &0u32.to_le_bytes());
        assert!(!read_setting_at(&key.0));
        key.set_raw(REG_DWORD, &2u32.to_le_bytes());
        assert!(!read_setting_at(&key.0), "2 is not on");
        key.set_raw(REG_DWORD, &u32::MAX.to_le_bytes());
        assert!(!read_setting_at(&key.0), "-1 is not on");
    }

    #[test]
    fn setting_string_is_off() {
        let key = TestKey::new("sz");
        let one: Vec<u8> = "1\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        key.set_raw(REG_SZ, &one);
        assert!(!read_setting_at(&key.0), "the string \"1\" is not on");
    }

    #[test]
    fn setting_qword_is_off() {
        let key = TestKey::new("qword");
        key.set_raw(REG_QWORD, &1u64.to_le_bytes());
        assert!(!read_setting_at(&key.0), "a QWORD 1 is not on");
    }

    // ---- claim or forward ----

    use pipe::tests::{exclusive, test_name, Serving};
    // `LaunchMode::*` above brings a `Claim` variant into scope.
    use super::Claim as Outcome;

    fn identity() -> Identity {
        own_identity().expect("own_identity")
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn file_args(path: &str, anchor: Option<&str>, find: Option<&str>) -> Args {
        Args { files: vec![PathBuf::from(path)], anchor: anchor.map(String::from), find: find.map(String::from), ..Args::default() }
    }

    /// The forwarded request, as the receiving test server recorded it.
    fn received(serving: &Serving) -> Request {
        serving.commits.recv_timeout(Duration::from_secs(3)).expect("no request reached the receiver")
    }

    // Every claim test holds the exclusive guard of the pipe tests: `PENDING` is process-wide, and `shutdown` in
    // those tests sets `CLOSING`, which would make `create_pipe` fail here.

    #[test]
    fn claim_first_receives_second_forwards_absolute_path_anchor_and_find() {
        let _g = exclusive();
        let _ = take_pending();
        let name = test_name();
        let id = identity();
        let abs = std::env::temp_dir().join("mdread-claim-a.md");
        let args = file_args(abs.to_str().unwrap(), Some("intro"), Some("a b"));
        assert_eq!(claim_or_forward_named(&name, &id, &args, LaunchMode::ForwardOrClaim, soon()), Outcome::Receiver);
        let pipe = take_pending().expect("the receiver's pipe waits in PENDING");
        assert!(take_pending().is_none(), "taken once");
        let serving = Serving::serve(pipe, true);
        assert_eq!(claim_or_forward_named(&name, &id, &args, LaunchMode::ForwardOrClaim, soon()), Outcome::Forwarded);
        let r = received(&serving);
        assert_eq!(r, Request { path: abs, anchor: Some("intro".into()), find: Some("a b".into()) });
        assert!(take_pending().is_none(), "a forwarding launch holds no pipe");
    }

    #[test]
    fn claim_relative_file_is_made_absolute_against_the_current_dir() {
        let _g = exclusive();
        let _ = take_pending();
        let name = test_name();
        let id = identity();
        let serving = Serving::start(&name, true);
        let args = file_args(r"notes\rel.md", None, None);
        assert_eq!(claim_or_forward_named(&name, &id, &args, LaunchMode::ForwardOrClaim, soon()), Outcome::Forwarded);
        let r = received(&serving);
        assert!(r.path.is_absolute(), "{}", r.path.display());
        assert_eq!(r.path, std::env::current_dir().unwrap().join(r"notes\rel.md"));
        assert_eq!((r.anchor, r.find), (None, None));
    }

    #[test]
    fn claim_empty_find_and_anchor_travel_as_none() {
        let _g = exclusive();
        let _ = take_pending();
        let name = test_name();
        let id = identity();
        let serving = Serving::start(&name, true);
        let args = file_args(r"C:\a.md", Some(""), Some(""));
        assert_eq!(claim_or_forward_named(&name, &id, &args, LaunchMode::ForwardOrClaim, soon()), Outcome::Forwarded);
        let r = received(&serving);
        assert_eq!((r.anchor, r.find), (None, None));
    }

    #[test]
    fn claim_request_uses_only_the_first_file() {
        let mut args = file_args(r"C:\first.md", Some("x"), Some("y"));
        args.files.push(PathBuf::from(r"C:\second.md"));
        assert_eq!(request_for(&args), Some(Request { path: PathBuf::from(r"C:\first.md"), anchor: Some("x".into()), find: Some("y".into()) }));
        assert_eq!(request_for(&Args::default()), None);
    }

    #[test]
    fn claim_mode_when_taken_is_standalone_and_when_free_receives() {
        let _g = exclusive();
        let _ = take_pending();
        let name = test_name();
        let id = identity();
        let holder = create_pipe(&name, &id).expect("create_pipe");
        assert_eq!(claim_or_forward_named(&name, &id, &Args::default(), LaunchMode::Claim, soon()), Outcome::Standalone);
        assert!(take_pending().is_none());
        drop(holder);
        assert_eq!(claim_or_forward_named(&name, &id, &Args::default(), LaunchMode::Claim, soon()), Outcome::Receiver);
        let pipe = take_pending().expect("pending pipe");
        assert!(matches!(create_pipe(&name, &id), Err(CreateError::Taken)), "the claimed pipe holds the name");
        drop(pipe);
    }

    #[test]
    fn claim_standalone_mode_never_creates_a_pipe() {
        let _g = exclusive();
        let _ = take_pending();
        let name = test_name();
        let id = identity();
        let args = file_args(r"C:\a.md", None, None);
        assert_eq!(claim_or_forward_named(&name, &id, &args, LaunchMode::Standalone, soon()), Outcome::Standalone);
        assert!(take_pending().is_none());
        let pipe = create_pipe(&name, &id).expect("the name is still free");
        drop(pipe);
    }

    #[test]
    fn claim_refusing_receiver_makes_the_sender_stand_alone_within_the_deadline() {
        let _g = exclusive();
        let _ = take_pending();
        let name = test_name();
        let id = identity();
        let serving = Serving::start(&name, false);
        let args = file_args(r"C:\a.md", None, None);
        let started = Instant::now();
        let claim = claim_or_forward_named(&name, &id, &args, LaunchMode::ForwardOrClaim, started + Duration::from_millis(600));
        let took = started.elapsed();
        assert_eq!(claim, Outcome::Standalone);
        assert!(took < Duration::from_millis(1500), "took {took:?}");
        assert!(take_pending().is_none());
        assert_eq!(serving.commits.try_recv().ok(), None, "nothing was committed");
    }

    #[test]
    fn claim_receiver_that_keeps_refusing_is_given_up_long_before_the_deadline() {
        // A hung receiver (or one with every worker busy) keeps the name and refuses every time. The sender must not
        // spend the whole 10 s deadline retrying: it gives up after a short run of refusals and opens its own window.
        let _g = exclusive();
        let _ = take_pending();
        let name = test_name();
        let id = identity();
        let serving = Serving::start(&name, false);
        let args = file_args(r"C:\a.md", None, None);
        let started = Instant::now();
        let claim = claim_or_forward_named(&name, &id, &args, LaunchMode::ForwardOrClaim, started + Duration::from_secs(10));
        let took = started.elapsed();
        assert_eq!(claim, Outcome::Standalone);
        assert!(took >= REFUSED_RETRY, "gave up after {took:?}, before retrying for a while");
        assert!(took < Duration::from_millis(1500), "took {took:?}");
        assert!(take_pending().is_none());
        assert_eq!(serving.commits.try_recv().ok(), None, "nothing was committed");
    }

    #[test]
    fn claim_empty_path_stands_alone_without_touching_the_pipe() {
        let _g = exclusive();
        let _ = take_pending();
        let name = test_name();
        let id = identity();
        // A receiver that would take anything: the sender must not even ask it.
        let serving = Serving::start(&name, true);
        let started = Instant::now();
        let claim = claim_or_forward_named(&name, &id, &file_args("", None, None), LaunchMode::ForwardOrClaim, started + Duration::from_secs(10));
        assert_eq!(claim, Outcome::Standalone);
        assert!(started.elapsed() < Duration::from_millis(200), "took {:?}", started.elapsed());
        assert!(take_pending().is_none());
        assert_eq!(serving.commits.try_recv().ok(), None, "nothing was sent");
    }

    #[test]
    fn claim_after_a_refusing_receiver_goes_away_receives() {
        // A receiver that is closing refuses, then frees the name: the sender claims it and becomes the receiver.
        let _g = exclusive();
        let _ = take_pending();
        let name = test_name();
        let id = identity();
        let serving = Serving::start(&name, false);
        let args = file_args(r"C:\a.md", None, None);
        let server = serving.server();
        let claim = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                server.stop_serving();
            });
            claim_or_forward_named(&name, &id, &args, LaunchMode::ForwardOrClaim, Instant::now() + Duration::from_secs(10))
        });
        assert_eq!(claim, Outcome::Receiver);
        let pipe = take_pending();
        assert!(pipe.is_some(), "the new receiver's pipe waits in PENDING");
        drop(pipe);
    }

    #[test]
    fn setting_write_to_impossible_subkey_is_an_error() {
        // A key name is limited to 255 characters.
        // The call creates the parent key before it fails on the long name, so the guard owns the parent.
        let parent = TestKey::new("long");
        let sub = format!(r"{}\{}", parent.0, "a".repeat(300));
        let e = write_setting_at(&sub, true).unwrap_err();
        assert!(e.raw_os_error().is_some_and(|c| c != 0), "{e}");
        assert!(!read_setting_at(&sub));
    }
}
