//! Windows-only startup plumbing. Release builds use the GUI subsystem and have no
//! console, so CLI output borrows the parent's console, and startup errors and panics go
//! to `%LOCALAPPDATA%\mdread\crash.log` and a message box. Also arms the glow -> wgpu
//! renderer fallback and picks the first-run window geometry. Also small shell and
//! locale helpers for opening links and decoding text, the title bar theme and
//! shortcut (.lnk) resolution.

use std::ffi::{c_void, OsStr, OsString};
use std::io::Write;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, RECT, SYSTEMTIME, S_OK, WPARAM};
use windows_sys::Win32::Globalization::GetACP;
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, STGM_READ};
use windows_sys::Win32::System::Console::{AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems, ShellLink};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, MessageBoxW, SetWindowLongPtrW, SystemParametersInfoW, GWLP_WNDPROC, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND,
    SPI_GETWORKAREA, WM_SETTINGCHANGE, WNDPROC,
};

/// `MDREAD_RENDERER=glow|wgpu` (case-insensitive) forces a renderer and disables the
/// fallback. Other values are ignored with a warning, keeping the fallback armed.
pub const RENDERER_ENV: &str = "MDREAD_RENDERER";

/// True while glow is starting and a failure should be retried with wgpu. Cleared once
/// the app has been created, i.e. the renderer is up.
static FALLBACK_ARMED: AtomicBool = AtomicBool::new(false);

pub fn arm_renderer_fallback(on: bool) {
    FALLBACK_ARMED.store(on, Ordering::SeqCst);
}

/// Disarms the fallback; returns whether it was still armed (the renderer never came up).
pub fn disarm_renderer_fallback() -> bool {
    FALLBACK_ARMED.swap(false, Ordering::SeqCst)
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// Makes stdout/stderr usable for CLI output. A GUI-subsystem exe started from a terminal
/// gets no standard handles unless they are redirected, so attach to the parent's
/// console. Only done right before printing and exiting: a process attached to a console
/// is killed by Ctrl+C and by closing that console. Returns false when there is nowhere
/// to write to (started from Explorer or a shortcut).
pub fn attach_console() -> bool {
    let usable = |h: HANDLE| !h.is_null() && h != INVALID_HANDLE_VALUE;
    unsafe {
        if usable(GetStdHandle(STD_OUTPUT_HANDLE)) || usable(GetStdHandle(STD_ERROR_HANDLE)) {
            return true;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return false;
        }
        // Rust looks the handles up on every write; fill in any that attaching left empty.
        for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            if !usable(GetStdHandle(id)) {
                let h = CreateFileW(
                    wide(OsStr::new("CONOUT$")).as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                if usable(h) {
                    SetStdHandle(id, h);
                }
            }
        }
    }
    true
}

/// Shows a message box. Runs on its own thread so the box's modal loop does not dispatch
/// messages to the winit windows of the calling thread.
pub fn message_box(text: &str, error: bool) {
    let text = wide(OsStr::new(text));
    let flags = MB_OK | MB_SETFOREGROUND | if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    let _ = std::thread::spawn(move || unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), wide(OsStr::new("mdread")).as_ptr(), flags);
    })
    .join();
}

/// `%LOCALAPPDATA%\mdread`, for the crash and debug logs.
pub fn data_dir() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData").join("Local")))?;
    Some(local.join("mdread"))
}

/// Opens a file under [`data_dir`] for appending.
pub fn open_log(name: &str) -> Option<(std::fs::File, PathBuf)> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(name);
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    Some((file, path))
}

/// Local time as `2026-10-19 14:03:07`.
pub fn timestamp() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn append_crash_log(details: &str) -> Option<PathBuf> {
    let (mut f, path) = open_log("crash.log")?;
    let _ = writeln!(f, "[{}] mdread {} (pid {})\n{}\n", timestamp(), env!("CARGO_PKG_VERSION"), std::process::id(), details.trim_end());
    Some(path)
}

/// Reports a fatal error: crash log, then a message box (the box blocks). Release builds
/// only; debug builds have a console that already shows the error.
pub fn report_fatal(summary: &str, details: &str) {
    if cfg!(debug_assertions) {
        return;
    }
    let text = match append_crash_log(details) {
        Some(path) => format!("{summary}\n\nDetails were written to {}", path.display()),
        None => summary.to_owned(),
    };
    message_box(&text, true);
}

/// Panics abort in release builds and nothing reaches stderr, so log them and tell the
/// user. A panic while glow is still starting (some drivers make it `expect`/`unwrap`
/// instead of returning an error) relaunches mdread with wgpu instead.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if disarm_renderer_fallback() && relaunch_with_wgpu() {
            if !cfg!(debug_assertions) {
                append_crash_log(&format!("glow renderer failed to start, relaunched with wgpu: {info}"));
            }
            std::process::exit(0);
        }
        default_hook(info);
        let backtrace = std::backtrace::Backtrace::force_capture();
        report_fatal(&format!("mdread crashed: {}", info.payload_as_str().unwrap_or("panic")), &format!("{info}\n{backtrace}"));
    }));
}

fn relaunch_with_wgpu() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    std::process::Command::new(exe).args(std::env::args_os().skip(1)).env(RENDERER_ENV, "wgpu").spawn().is_ok()
}

/// First-run window position and inner size (logical points): `default` shrunk to fit the
/// primary monitor's work area (without the taskbar) and centered in it. Persisted
/// geometry replaces this on later runs.
pub fn first_run_geometry(default: egui::Vec2) -> Option<(egui::Pos2, egui::Vec2)> {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut r).cast(), 0) } == 0 {
        return None;
    }
    // Physical pixels in a DPI-aware process (the manifest says so), else already scaled.
    let scale = unsafe { GetDpiForSystem() }.max(96) as f32 / 96.0;
    let work = egui::Rect::from_min_max(
        egui::pos2(r.left as f32, r.top as f32) / scale,
        egui::pos2(r.right as f32, r.bottom as f32) / scale,
    );
    (work.width() > 0.0 && work.height() > 0.0).then(|| fit_window(work, default))
}

/// Room left around the window, and the height of the title bar on top of the inner size.
const MARGIN: f32 = 16.0;
const TITLE_BAR: f32 = 32.0;

fn fit_window(work: egui::Rect, default: egui::Vec2) -> (egui::Pos2, egui::Vec2) {
    let max = egui::vec2(work.width() - 2.0 * MARGIN, work.height() - 2.0 * MARGIN - TITLE_BAR);
    let size = default.min(max).max(egui::vec2(420.0, 300.0));
    let outer = size + egui::vec2(0.0, TITLE_BAR);
    let pos = work.min + ((work.size() - outer) / 2.0).max(egui::Vec2::ZERO);
    (pos.round(), size.round())
}

/// The system ANSI code page (1252 on Western systems), for text files that are not UTF-8.
pub fn ansi_code_page() -> u32 {
    unsafe { GetACP() }
}

/// Initializes COM (single-threaded apartment) on the current thread for as long as it
/// lives. Shell calls need it: ShellExecuteExW hands some file types to COM handlers.
pub struct ComApartment(bool);

impl ComApartment {
    pub fn init() -> Self {
        let hr = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
        Self(hr >= 0)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Opens the folder containing `path` in Explorer with the file selected, without
/// running the file. Needs COM on the calling thread ([`ComApartment`]).
pub fn reveal_in_explorer(path: &std::path::Path) -> std::io::Result<()> {
    let wide_path = wide(dunce::simplified(path).as_os_str());
    unsafe {
        let pidl = ILCreateFromPathW(wide_path.as_ptr());
        if pidl.is_null() {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "Explorer cannot find the file"));
        }
        // With no child items, the folder PIDL itself names the one item to select.
        let hr = SHOpenFolderAndSelectItems(pidl, 0, std::ptr::null(), 0);
        ILFree(pidl);
        if hr < 0 {
            return Err(std::io::Error::from_raw_os_error(hr));
        }
    }
    Ok(())
}

/// Whether the title bar should be dark, for the window procedure wrapper below.
static TITLE_BAR_DARK: AtomicBool = AtomicBool::new(false);
/// winit's window procedure, which the wrapper calls first.
static WINIT_WNDPROC: AtomicIsize = AtomicIsize::new(0);

/// Makes the title bar dark or light to match the app's theme (the `t` key, `--dark`,
/// `--light`) rather than the system's. winit puts the system theme back on every
/// WM_SETTINGCHANGE broadcast, which is any changed setting, not only the theme, and
/// without telling the app. So the window procedure is wrapped once to set the app's
/// theme again right after winit has handled the message. mdread has one window.
pub fn set_title_bar_dark(hwnd: isize, dark: bool) {
    let hwnd = hwnd as HWND;
    TITLE_BAR_DARK.store(dark, Ordering::Relaxed);
    if WINIT_WNDPROC.load(Ordering::Relaxed) == 0 {
        let wrapper: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT = title_bar_wndproc;
        let prev = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wrapper as isize) };
        WINIT_WNDPROC.store(prev, Ordering::Relaxed);
    }
    apply_title_bar(hwnd, dark);
}

fn apply_title_bar(hwnd: HWND, dark: bool) {
    let value: i32 = dark.into();
    let set = |attr: i32| unsafe { DwmSetWindowAttribute(hwnd, attr as u32, (&raw const value).cast(), size_of::<i32>() as u32) };
    // Windows 10 before build 18985 knows the attribute under its preview number, 19.
    if set(DWMWA_USE_IMMERSIVE_DARK_MODE) < 0 {
        set(19);
    }
}

unsafe extern "system" fn title_bar_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let prev = WINIT_WNDPROC.load(Ordering::Relaxed);
    if prev == 0 {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let prev: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(prev) };
    let result = unsafe { CallWindowProcW(prev, hwnd, msg, wparam, lparam) };
    if msg == WM_SETTINGCHANGE {
        apply_title_bar(hwnd, TITLE_BAR_DARK.load(Ordering::Relaxed));
    }
    result
}

/// An owned COM interface pointer, called through its vtable (windows-sys has no
/// interface definitions).
struct ComPtr(*mut c_void);

impl ComPtr {
    /// The function in vtable slot `slot`; `F` must be its signature.
    unsafe fn method<F: Copy>(&self, slot: usize) -> F {
        unsafe {
            let vtable = *(self.0 as *const *const usize);
            std::mem::transmute_copy(&*vtable.add(slot))
        }
    }

    fn query(&self, iid: &GUID) -> Option<ComPtr> {
        type QueryInterface = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32;
        let mut out = std::ptr::null_mut();
        let hr = unsafe { self.method::<QueryInterface>(0)(self.0, iid, &mut out) };
        (hr >= 0 && !out.is_null()).then_some(ComPtr(out))
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        type Release = unsafe extern "system" fn(*mut c_void) -> u32;
        unsafe { self.method::<Release>(2)(self.0) };
    }
}

const IID_ISHELLLINKW: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
const IID_IPERSISTFILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);
/// Vtable slots: IShellLinkW::GetPath and SetPath, IPersistFile::Load and Save.
const SHELL_LINK_GET_PATH: usize = 3;
#[cfg(test)]
const SHELL_LINK_SET_PATH: usize = 20;
const PERSIST_FILE_LOAD: usize = 5;
#[cfg(test)]
const PERSIST_FILE_SAVE: usize = 6;

fn shell_link() -> Option<ComPtr> {
    let mut link = std::ptr::null_mut();
    let hr = unsafe { CoCreateInstance(&ShellLink, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ISHELLLINKW, &mut link) };
    (hr >= 0 && !link.is_null()).then_some(ComPtr(link))
}

/// The file or folder a shortcut (.lnk) points to. Only reads the path stored in the
/// shortcut: a moved target is not searched for, so this never blocks on the network.
/// None for shortcuts to things that are not files, such as Control Panel items. Needs
/// COM on the calling thread ([`ComApartment`]).
pub fn shortcut_target(lnk: &Path) -> Option<PathBuf> {
    type Load = unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32;
    type GetPath = unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> i32;
    let link = shell_link()?;
    let file = link.query(&IID_IPERSISTFILE)?;
    let mut buf = vec![0u16; 32768];
    unsafe {
        if file.method::<Load>(PERSIST_FILE_LOAD)(file.0, wide(lnk.as_os_str()).as_ptr(), STGM_READ) < 0 {
            return None;
        }
        // S_FALSE when the shortcut has no file system path.
        if link.method::<GetPath>(SHELL_LINK_GET_PATH)(link.0, buf.as_mut_ptr(), buf.len() as i32, std::ptr::null_mut(), 0) != S_OK {
            return None;
        }
    }
    let len = buf.iter().position(|&c| c == 0)?;
    (len > 0).then(|| PathBuf::from(OsString::from_wide(&buf[..len])))
}

#[cfg(test)]
fn create_shortcut(lnk: &Path, target: &Path) -> bool {
    type SetPath = unsafe extern "system" fn(*mut c_void, *const u16) -> i32;
    type Save = unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32;
    let Some(link) = shell_link() else { return false };
    let Some(file) = link.query(&IID_IPERSISTFILE) else { return false };
    unsafe {
        link.method::<SetPath>(SHELL_LINK_SET_PATH)(link.0, wide(target.as_os_str()).as_ptr()) >= 0
            && file.method::<Save>(PERSIST_FILE_SAVE)(file.0, wide(lnk.as_os_str()).as_ptr(), 1) >= 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_target_reads_the_stored_path() {
        let _com = ComApartment::init();
        let dir = std::env::temp_dir().join(format!("mdread-lnk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("notes.md");
        std::fs::write(&target, "# Notes\n").unwrap();
        let lnk = dir.join("notes.md.lnk");
        assert!(create_shortcut(&lnk, &target));
        assert_eq!(shortcut_target(&lnk), Some(target));
        assert_eq!(shortcut_target(&dir.join("missing.lnk")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
    }

    #[test]
    fn fit_window_keeps_default_on_large_screens() {
        // 2560x1440 at 100 %, 48 px taskbar.
        let (pos, size) = fit_window(rect(0.0, 0.0, 2560.0, 1392.0), egui::vec2(1500.0, 1250.0));
        assert_eq!(size, egui::vec2(1500.0, 1250.0));
        assert_eq!(pos, egui::pos2(530.0, 55.0));
    }

    #[test]
    fn fit_window_shrinks_to_small_work_areas() {
        // 1366x768 at 100 % and 1920x1080 at 150 %, each with a 40/48 px taskbar.
        for work in [rect(0.0, 0.0, 1366.0, 728.0), rect(0.0, 0.0, 1280.0, 688.0)] {
            let (pos, size) = fit_window(work, egui::vec2(1500.0, 1250.0));
            assert!(work.contains_rect(egui::Rect::from_min_size(pos, size + egui::vec2(0.0, TITLE_BAR))), "{work:?}: {pos:?} {size:?}");
            assert_eq!(size.x, work.width() - 2.0 * MARGIN);
        }
    }

    #[test]
    fn fit_window_respects_work_area_offset() {
        // Taskbar at the top or left moves the work area's origin.
        let (pos, _) = fit_window(rect(0.0, 48.0, 1366.0, 720.0), egui::vec2(1500.0, 1250.0));
        assert_eq!(pos, egui::pos2(MARGIN, 48.0 + MARGIN));
    }
}
