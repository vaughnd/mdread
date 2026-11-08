//! Application state, input handling and the top-level layout
//! (outline sidebar · reading column · file sidebar · status bar).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use egui::scroll_area::{DragScroll, ScrollSource};
use egui::style::ScrollAnimation;
use egui::text::{CCursor, CCursorRange};
use egui::text_selection::LabelSelectionState;
use egui::{
    pos2, vec2, Align, Align2, Button, Color32, Context, CornerRadius, Event, Frame, Id, Key, Label, LayerId, Layout,
    Margin, Modifiers, Panel, PointerButton, Popup, PopupAnchor, PopupCloseBehavior, PopupKind, Pos2, Rect, RichText,
    ScrollArea, Sense, Shape, Stroke, TextEdit, Ui, UiBuilder, ViewportCommand,
};
use notify::Watcher;
use serde::{Deserialize, Serialize};

use crate::document::{Block, Document, InlineItem};
use crate::files;
use crate::fonts::{self, FontSet};
use crate::highlight::{self, Highlighter};
use crate::links::{self, Target};
use crate::mermaid::Diagrams;
use crate::render::{Action, RenderCtx};
use crate::search::SearchState;
use crate::text;
use crate::theme::{self, Palette};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    System,
    Dark,
    Light,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub zoom: f32,
    pub base_size: f32,
    pub column_width: f32,
    pub full_width: bool,
    pub show_outline: bool,
    pub show_files: bool,
    pub theme: ThemeChoice,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            base_size: 16.5,
            column_width: 860.0,
            full_width: false,
            show_outline: true,
            show_files: true,
            theme: ThemeChoice::System,
        }
    }
}

/// Reading-column widths offered in the status bar, in points.
const WIDTH_PRESETS: [(&str, f32); 4] = [("Narrow", 720.0), ("Normal", 860.0), ("Wide", 1200.0), ("Extra wide", 1600.0)];

/// Bounds of `Settings::column_width`, in points.
const MIN_COLUMN: f32 = 400.0;
const MAX_COLUMN: f32 = 3000.0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum WidthStep {
    Preset(f32),
    Full,
}

/// A small button that shows its frame only when hovered, for the status bar and the
/// sidebar header.
fn flat_button(text: RichText) -> Button<'static> {
    Button::new(text).frame_when_inactive(false).small()
}

/// An arrow for a `flat_button`, in the code font: the usual UI fonts (Ubuntu Sans,
/// egui's own) have no arrows, and code fonts do.
fn arrow(text: &str, size: f32, color: Color32) -> RichText {
    RichText::new(text).size(size).color(color).family(egui::FontFamily::Monospace)
}

/// The last part of `dir`'s path, or all of it for a root (`/`, `C:\`).
fn folder_name(dir: &Path) -> String {
    dir.file_name().map_or_else(|| links::display_path(dir), |n| n.to_string_lossy().into_owned())
}

/// Where the status bar's − or + button goes from a column `now` points wide: the next
/// narrower preset, or the next wider one that fits in `column_max` and then full width.
fn width_step(now: f32, full_width: bool, column_max: f32, wider: bool) -> Option<WidthStep> {
    let mut presets = WIDTH_PRESETS.iter().map(|p| p.1);
    if !wider {
        presets.rev().find(|&w| w < now - 1.0).map(WidthStep::Preset)
    } else if full_width {
        None
    } else {
        Some(presets.find(|&w| w > now + 1.0 && w < column_max - 1.0).map_or(WidthStep::Full, WidthStep::Preset))
    }
}

#[derive(Clone, Default)]
pub struct Args {
    pub files: Vec<PathBuf>,
    pub stdin: bool,
    pub theme: Option<ThemeChoice>,
    /// Debug aid: save a PNG of the window after `screenshot_delay_ms` and exit.
    pub screenshot: Option<PathBuf>,
    pub screenshot_delay_ms: u64,
    pub launched: Option<Instant>,
    /// `#anchor` given after the file name on the command line.
    pub anchor: Option<String>,
    /// `--find TERM`: open with the search box filled in.
    pub find: Option<String>,
    /// `--new-window` / `--same-window`, whichever came last. Only the single-instance
    /// code on Windows reads it; the flags are accepted everywhere so scripts stay portable.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub window: Option<WindowChoice>,
}

/// Where a launch with a file wants to open, when it says so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum WindowChoice {
    /// `--new-window`: a window of its own even when the setting would send the file to the open one.
    NewWindow,
    /// `--same-window`: the open mdread window, if there is one.
    SameWindow,
}

enum WatchEvent {
    /// The open file changed.
    Changed,
    /// The file changed without a watcher event (Windows): notify silently stops watching
    /// after an error or a buffer overflow, so start a new watcher.
    #[cfg_attr(not(windows), allow(dead_code))]
    Missed,
}

/// Size and modification time of the open file.
pub(crate) type FileStamp = (u64, SystemTime);

pub(crate) fn file_stamp(path: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// Reads a document to open: the path it is shown under, and its decoded text with the
/// file's stamp taken before the read (or the read error). dunce leaves out the `\\?\`
/// prefix Windows puts on canonical paths. Some virtual drives cannot be canonicalized; an
/// absolute path still gives the folder links and the sidebar start from, and it is what
/// an error message names.
fn read_for_open(path: &Path) -> (PathBuf, std::io::Result<(text::Decoded, Option<FileStamp>)>) {
    let path = dunce::canonicalize(path).or_else(|_| std::path::absolute(path)).unwrap_or_else(|_| path.to_path_buf());
    let stamp = file_stamp(&path);
    let read = std::fs::read(&path).map(|bytes| (text::decode(bytes), stamp));
    (path, read)
}

/// How often the open file's stamp is checked on Windows, as a fallback for the watcher.
#[cfg(windows)]
const STAMP_POLL: Duration = Duration::from_secs(2);

/// A file in the back/forward history, and how it was left.
#[derive(Clone)]
struct Visit {
    path: PathBuf,
    offset: f32,
    /// The folder the files sidebar showed; `None` until the file is left.
    files_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Default)]
struct Measure {
    height: f32,
    measured: bool,
    /// The block drew an image or diagram that was still loading, so `height` has its
    /// placeholder's size rather than its own.
    pending: bool,
}

/// The right-click menu over the reading column.
///
/// Its open state lives here rather than in egui's popup memory because the
/// right-press that opens it is swallowed in `raw_input_hook` before egui sees it:
/// egui collapses the label text selection on any press over a label, which would
/// leave nothing for "Copy" to copy.
struct ContextMenu {
    pos: Pos2,
    link: Option<String>,
    image: Option<String>,
    code: Option<Arc<str>>,
    /// Popup rect from the last frame; presses outside it dismiss the menu.
    rect: Rect,
    /// The enabled "Copy" item's rect; a press on it becomes an `Event::Copy`.
    copy_rect: Option<Rect>,
}

#[derive(Clone, Copy)]
enum MenuItem {
    OpenLink,
    CopyLink,
    OpenImage,
    CopyImage,
    CopyCode,
}

pub struct App {
    settings: Settings,
    fonts: FontSet,
    pal: Palette,
    system_dark: Option<bool>,

    doc: Option<Arc<Document>>,
    gen: u64,
    path: Option<PathBuf>,
    source: String,
    error: Option<String>,
    stdin_mode: bool,

    measures: Vec<Measure>,
    block_tops: Vec<f32>,
    layout_key: (i32, i32),
    measure_all: bool,

    scroll_offset: f32,
    viewport_h: f32,
    content_h: f32,
    /// The document generation `content_h` was measured for.
    content_gen: u64,
    pending_offset: Option<f32>,
    scroll_delta: f32,
    jump: Option<(usize, u8)>,
    /// Until when a jump waits for diagrams and images above its target to load.
    jump_until: Instant,
    pending_anchor: Option<String>,

    search: SearchState,
    hl: Highlighter,
    diagrams: Diagrams,
    hovered_link: Option<(usize, u32)>,

    /// Reading-column viewport from the last frame, in points.
    content_rect: Rect,
    /// Widest the reading column fits in the window, from the last frame, in points.
    column_max: f32,
    menu: Option<ContextMenu>,
    /// Right-press position waiting for the next frame's hover info.
    menu_request: Option<Pos2>,
    /// A press was swallowed in `raw_input_hook`; swallow its release too.
    swallow_release: Option<PointerButton>,

    history: Vec<Visit>,
    history_pos: usize,

    watcher: Option<notify::RecommendedWatcher>,
    watch_rx: Option<mpsc::Receiver<WatchEvent>>,
    /// Stops the stamp poller of the previous watcher.
    watch_alive: Arc<AtomicBool>,
    /// Stamp of the file as last read.
    seen: Arc<egui::mutex::Mutex<Option<FileStamp>>>,
    reload_at: Option<Instant>,
    reload_retries: u32,
    dialog_rx: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// Errors from opening links in other apps.
    launch_tx: mpsc::Sender<String>,
    launch_rx: mpsc::Receiver<String>,

    files: Vec<PathBuf>,
    files_dir: Option<PathBuf>,
    files_rx: Option<mpsc::Receiver<Vec<PathBuf>>>,

    status: Option<(String, Instant)>,
    show_help: bool,
    current_heading: Option<usize>,
    outline_scroll_to: Option<usize>,
    started: Instant,
    frames: u64,
    /// Theme to persist: the CLI `--dark/--light` override is session-only.
    theme_override: Option<ThemeChoice>,
    saved_theme: ThemeChoice,
    screenshot: Option<PathBuf>,
    screenshot_delay_ms: u64,
    /// Where the screenshot lands once requested and taken.
    screenshot_taken: Option<Arc<egui::mutex::Mutex<Option<Arc<egui::ColorImage>>>>>,
    /// The screenshot was requested at the wait limit, before the page had finished drawing.
    screenshot_unsettled: bool,
    /// Whether the title bar was last made dark (Windows; None before the first frame).
    #[cfg(windows)]
    title_bar_dark: Option<bool>,

    /// The ticket of the last navigation on the UI thread (`load_path_in`, failed or not,
    /// which `go_history` goes through, and the stdin load). A forward result committed
    /// before it is dropped, so a slow forward never replaces what the user opened since.
    /// Applying a forward does not change it: forwards apply in the order their results
    /// reach the UI thread, each pushed on history, the last one shown (an Explorer
    /// multi-select opens every file, not only the first to be read).
    #[cfg_attr(not(windows), allow(dead_code))]
    nav_gen: u64,
    /// Where tickets come from, shared with the pipe thread, which takes one when a request
    /// is committed, so "committed before the last navigation" is one comparison.
    open_tickets: Arc<AtomicU64>,
    /// A forwarded file applied this frame, logged once the frame has drawn it.
    #[cfg(windows)]
    forward_shown: Option<(PathBuf, Instant)>,
    /// Forward workers send their results here; `poll_channels` applies them.
    #[cfg(windows)]
    forward_tx: mpsc::Sender<crate::instance::ForwardResult>,
    #[cfg(windows)]
    forward_rx: mpsc::Receiver<crate::instance::ForwardResult>,
    /// The window, as an integer so the pipe thread's closures can hold it; None without
    /// a native window (the headless tests).
    #[cfg(windows)]
    hwnd: Option<isize>,
    /// Started with `--same-window`: such a window keeps receiving when the setting is
    /// turned off in it, since the user asked for this window explicitly.
    #[cfg(windows)]
    started_same_window: bool,
    /// Started for stdin or `--screenshot`: never receives, not even when the setting is
    /// turned on in it: its page is not a file, or it is about to exit.
    #[cfg(windows)]
    never_receives: bool,
    /// The pipe this window serves, if it receives.
    #[cfg(windows)]
    server: Option<crate::instance::Server>,
    /// A server stopped by turning the setting off whose thread had not finished within
    /// `stop_serving`'s wait; kept so turning the setting on again can tell whether the
    /// name is free yet.
    #[cfg(windows)]
    stopped_server: Option<crate::instance::Server>,
    /// The help window's checkbox: the setting as read when the help window was opened (one
    /// registry read per opening, not per frame), then as written by its clicks.
    #[cfg(windows)]
    setting_on: bool,
    /// Tests: a test pipe name instead of this exe's, and a test registry key instead of
    /// `Software\mdread`, so they never touch a real mdread's pipe or the real setting.
    #[cfg(all(windows, test))]
    pipe_name: Option<String>,
    #[cfg(all(windows, test))]
    setting_subkey: Option<String>,
}

/// At most this many forwarded files are read at once (each on its own worker thread). The
/// cap is checked in `accept`, before the sender is told its file is delivered: a sender
/// beyond it is refused, keeps its file and opens a window of its own, instead of more
/// threads getting stuck on a slow share.
#[cfg(windows)]
const MAX_FORWARD_WORKERS: usize = 8;

/// The receiver's `accept`: refuse while the window hangs (a file sent to it would wait
/// unseen) or while every worker place is taken (the file would have no worker to read it).
/// Only the pipe thread takes places, and it runs `accept` and `commit` back to back, so a
/// place free here is still free in `commit`.
#[cfg(windows)]
fn accepts_forward(in_flight: &AtomicUsize, hung: bool) -> bool {
    !hung && in_flight.load(Ordering::SeqCst) < MAX_FORWARD_WORKERS
}

/// One of the `MAX_FORWARD_WORKERS` places, held by a forward worker for as long as it runs
/// and given back when dropped, also if the thread could not be started.
#[cfg(windows)]
struct WorkerSlot(Arc<AtomicUsize>);

#[cfg(windows)]
impl WorkerSlot {
    /// A place, unless `max` are taken.
    fn take(count: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        // Taken first and given back when over, so two threads cannot both take the last place.
        if count.fetch_add(1, Ordering::SeqCst) >= max {
            count.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(count.clone()))
    }
}

#[cfg(windows)]
impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The next open ticket. Never 0, so a new window's `nav_gen` of 0 makes no ticket stale.
fn take_ticket(tickets: &AtomicU64) -> u64 {
    tickets.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
}

/// What a folder link opens, in this order.
const FOLDER_INDEX: &[&str] = &["README.md", "readme.md", "index.md", "README.markdown"];

/// Label for the primary shortcut modifier: egui's `Modifiers::COMMAND` is Cmd on macOS
/// and Ctrl everywhere else.
const CMD: &str = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" };

/// Shell command that prints the chosen file path on stdout, or nothing if cancelled.
/// macOS uses the native panel through `osascript`; elsewhere `zenity` (GTK). Windows
/// uses the native dialog through rfd instead.
#[cfg(not(windows))]
fn file_dialog_command(start_dir: Option<&Path>) -> std::process::Command {
    if cfg!(target_os = "macos") {
        let mut script = String::from(
            "POSIX path of (choose file with prompt \"Open Markdown file\" of type {\"md\", \"markdown\", \"mdown\", \"mkd\", \"mdx\", \"txt\", \"public.plain-text\"}",
        );
        if let Some(d) = start_dir {
            // AppleScript string literals: escape backslashes and double quotes.
            let d = d.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
            script.push_str(&format!(" default location POSIX file \"{d}\""));
        }
        script.push(')');
        let mut cmd = std::process::Command::new("osascript");
        cmd.arg("-e").arg(script);
        cmd
    } else {
        let mut cmd = std::process::Command::new("zenity");
        cmd.arg("--file-selection")
            .arg("--title=Open Markdown file")
            .arg("--file-filter=Markdown | *.md *.markdown *.mdown *.mkd *.mdx *.txt")
            .arg("--file-filter=All files | *");
        if let Some(d) = start_dir {
            cmd.arg(format!("--filename={}/", d.display()));
        }
        cmd
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        let ctx = &cc.egui_ctx;
        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
            .unwrap_or_default();
        let saved_theme = settings.theme;
        if let Some(t) = args.theme {
            settings.theme = t;
        }
        #[cfg(windows)]
        crate::svg_loader::install(ctx);
        egui_extras::install_image_loaders(ctx);
        highlight::preload();
        let fonts = fonts::install(ctx);
        ctx.set_zoom_factor(settings.zoom.clamp(0.5, 3.0));

        let system_dark = ctx.system_theme().map(|t| t == egui::Theme::Dark);
        let dark = match settings.theme {
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
            ThemeChoice::System => system_dark.unwrap_or(true),
        };
        let pal = if dark { Palette::dark() } else { Palette::light() };
        theme::apply(ctx, &pal, &fonts, settings.base_size);
        let (launch_tx, launch_rx) = mpsc::channel();
        #[cfg(windows)]
        let (forward_tx, forward_rx) = mpsc::channel();
        #[cfg(windows)]
        let hwnd = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(w)) => Some(w.hwnd.get()),
                _ => None,
            }
        };

        let mut app = Self {
            settings,
            fonts,
            pal,
            system_dark,
            doc: None,
            gen: 0,
            path: None,
            source: String::new(),
            error: None,
            stdin_mode: false,
            measures: Vec::new(),
            block_tops: Vec::new(),
            layout_key: (0, 0),
            measure_all: false,
            scroll_offset: 0.0,
            viewport_h: 800.0,
            content_h: 0.0,
            content_gen: 0,
            pending_offset: None,
            scroll_delta: 0.0,
            jump: None,
            jump_until: Instant::now(),
            pending_anchor: None,
            search: SearchState::default(),
            hl: Highlighter::new(ctx.clone()),
            diagrams: Diagrams::new(ctx.clone()),
            hovered_link: None,
            content_rect: Rect::NOTHING,
            column_max: f32::INFINITY,
            menu: None,
            menu_request: None,
            swallow_release: None,
            history: Vec::new(),
            history_pos: 0,
            watcher: None,
            watch_rx: None,
            watch_alive: Arc::new(AtomicBool::new(false)),
            seen: Arc::new(egui::mutex::Mutex::new(None)),
            reload_at: None,
            reload_retries: 0,
            dialog_rx: None,
            launch_tx,
            launch_rx,
            files: Vec::new(),
            files_dir: None,
            files_rx: None,
            status: None,
            show_help: false,
            current_heading: None,
            outline_scroll_to: None,
            started: args.launched.unwrap_or_else(Instant::now),
            frames: 0,
            theme_override: args.theme,
            saved_theme,
            screenshot: args.screenshot.clone(),
            screenshot_delay_ms: args.screenshot_delay_ms,
            screenshot_taken: None,
            screenshot_unsettled: false,
            #[cfg(windows)]
            title_bar_dark: None,
            nav_gen: 0,
            open_tickets: Arc::new(AtomicU64::new(0)),
            #[cfg(windows)]
            forward_shown: None,
            #[cfg(windows)]
            forward_tx,
            #[cfg(windows)]
            forward_rx,
            #[cfg(windows)]
            hwnd,
            #[cfg(windows)]
            started_same_window: args.window == Some(WindowChoice::SameWindow),
            #[cfg(windows)]
            never_receives: args.stdin || args.screenshot.is_some(),
            #[cfg(windows)]
            server: None,
            #[cfg(windows)]
            stopped_server: None,
            #[cfg(windows)]
            setting_on: false,
            #[cfg(all(windows, test))]
            pipe_name: None,
            #[cfg(all(windows, test))]
            setting_subkey: None,
        };

        // `--find` and the anchor belong to the document they were given with: both are
        // applied only after a successful load. With no file and no stdin, `--find` still
        // opens the search box.
        let mut loaded = false;
        if args.stdin {
            use std::io::Read;
            app.bump_nav_gen();
            app.stdin_mode = true;
            let mut bytes = Vec::new();
            match std::io::stdin().read_to_end(&mut bytes) {
                Ok(n) if n > 0 => {
                    let decoded = text::decode(bytes);
                    app.set_document(decoded.text, None, ctx);
                    if let Some(enc) = decoded.encoding {
                        app.set_status(format!("Decoded as {enc}"));
                    }
                    loaded = true;
                }
                Ok(_) => app.error = Some("No input on stdin".into()),
                Err(e) => app.error = Some(format!("Cannot read stdin: {e}")),
            }
        } else if let Some(p) = args.files.first() {
            loaded = app.open_command_line_path(p.clone(), ctx);
            if loaded {
                app.pending_anchor = args.anchor.clone();
            }
        }
        // With no file and no stdin there is nothing to load, and `--find` still opens the box.
        if loaded || (args.files.is_empty() && !args.stdin) {
            if let Some(q) = &args.find {
                app.search.open = true;
                app.search.query = q.clone();
                app.search.focus_pending = true;
            }
        }
        // Serving starts after the window's own file is loaded, so a forward that arrives
        // during startup gets a newer ticket and is shown, with the startup file on Back.
        #[cfg(windows)]
        if !args.stdin {
            if app.hwnd.is_some() {
                if let Some(pipe) = crate::instance::take_pending() {
                    app.start_serving(pipe, ctx);
                }
            } else {
                // No native window (never seen outside the headless tests): close a claimed
                // pipe, so senders find no receiver instead of waiting for one that never
                // answers. Not in tests, which must not take a pipe another test claimed.
                #[cfg(not(test))]
                if crate::instance::take_pending().is_some() {
                    log::warn!("single-window mode: no window handle; not receiving");
                }
            }
        }
        app
    }

    /// Serves the claimed pipe. Both closures run on the pipe thread and must return
    /// quickly without waiting for the UI thread and without panicking: `accept` asks
    /// Windows whether the window hangs and checks the worker cap (`accepts_forward`), and
    /// `commit` takes the open ticket, restores and focuses the window, and hands the
    /// request to a worker thread, which reads the file (off the UI thread, since a share
    /// can take tens of seconds) and sends the result to `poll_channels`. Without a window
    /// handle (only the headless tests serve so) there is no window to ask or to focus.
    #[cfg(windows)]
    fn start_serving(&mut self, pipe: crate::instance::PipeHandle, ctx: &Context) {
        use crate::instance::{resolve_forward, ForwardResult, Request};
        let hwnd = self.hwnd;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let accept = {
            let in_flight = in_flight.clone();
            Arc::new(move |_: &Request| accepts_forward(&in_flight, hwnd.is_some_and(crate::win::is_hung)))
        };
        let tickets = self.open_tickets.clone();
        let tx = self.forward_tx.clone();
        let ctx = ctx.clone();
        let commit = Arc::new(move |request: Request| {
            let arrived = Instant::now();
            let gen = take_ticket(&tickets);
            if let Some(hwnd) = hwnd {
                crate::win::bring_to_front(hwnd);
            }
            ctx.request_repaint();
            // The sender has been told the file is delivered, so a file that gets no worker
            // still gets a status instead of vanishing. `Sender::send` never blocks.
            let fail = |request: Request, msg: String| {
                log::warn!("forwarded {}: {msg}", request.path.display());
                if tx.send(ForwardResult { gen, request, outcome: Err(msg), arrived }).is_ok() {
                    ctx.request_repaint();
                }
            };
            // `accept` refused at the cap already; this is only a safety net.
            let Some(slot) = WorkerSlot::take(&in_flight, MAX_FORWARD_WORKERS) else {
                fail(request, "Too many files are being opened at once".into());
                return;
            };
            let (tx2, ctx2, fallback) = (tx.clone(), ctx.clone(), request.clone());
            let worker = std::thread::Builder::new().name("forward-open".into()).spawn(move || {
                let _slot = slot;
                let outcome = resolve_forward(&request);
                if tx2.send(ForwardResult { gen, request, outcome, arrived }).is_ok() {
                    ctx2.request_repaint();
                }
            });
            if worker.is_err() {
                let msg = format!("Cannot open {}: no worker thread available", links::display_path(&fallback.path));
                fail(fallback, msg);
            }
        });
        self.server = Some(pipe.serve(accept, commit));
    }

    /// The help window's checkbox. The value is written at once, by this window only, never
    /// from `app.ron` or on exit, so another window's save cannot undo it. On: this window
    /// starts receiving if no window does. Off: this window stops receiving, unless it was
    /// started with `--same-window`, which keeps listening (the user asked for it on the
    /// command line); a receiver in another window keeps its pipe until it closes.
    #[cfg(windows)]
    fn set_single_window(&mut self, on: bool, ctx: &Context) {
        if let Err(e) = self.write_setting(on) {
            log::warn!("single-window setting: cannot write it ({e})");
            self.set_status(format!("Cannot save the setting: {e}"));
            return;
        }
        self.setting_on = on;
        if on {
            self.start_receiving(ctx);
        } else if !self.started_same_window {
            if let Some(server) = self.server.take() {
                server.stop_serving();
                log::info!("single-window mode turned off: no longer receiving");
                if !server.is_finished() {
                    self.stopped_server = Some(server);
                }
            }
        }
    }

    /// Turning the setting on: claim the pipe exactly as `main` would, unless this window
    /// never receives, already receives, or still waits for a server it stopped earlier.
    /// A name another window holds means that window receives, and nothing more happens.
    #[cfg(windows)]
    fn start_receiving(&mut self, ctx: &Context) {
        use crate::instance::CreateError;
        // No native window: as in `App::new`, do not serve (never seen outside the tests).
        #[cfg(not(test))]
        if self.hwnd.is_none() {
            log::warn!("single-window mode: no window handle; not receiving");
            return;
        }
        if self.never_receives || self.server.as_ref().is_some_and(|s| !s.is_finished()) {
            return;
        }
        if self.stopped_server.as_ref().is_some_and(|s| !s.is_finished()) {
            self.set_status("Receiving starts with the next window");
            return;
        }
        self.stopped_server = None;
        self.server = None;
        // Low integrity and AppContainer windows have no identity to serve with: a pipe at
        // their level could be opened by other sandboxed processes.
        let (id, name) = match crate::instance::own_identity().and_then(|id| Ok((id, self.receiving_pipe_name()?))) {
            Ok(found) => found,
            Err(e) => {
                log::info!("single-window mode: not taking part ({e})");
                return;
            }
        };
        match crate::instance::create_pipe(&name, &id) {
            Ok(pipe) => {
                self.start_serving(pipe, ctx);
                log::info!("single-window mode turned on: receiving");
            }
            Err(CreateError::Taken) => log::info!("single-window mode turned on: another window receives"),
            Err(CreateError::Other(e)) => log::warn!("single-window pipe {name}: cannot create it ({e}); not receiving"),
        }
    }

    /// The setting as stored in the registry, for the checkbox when the help window opens.
    #[cfg(windows)]
    fn read_setting(&self) -> bool {
        #[cfg(test)]
        if let Some(key) = &self.setting_subkey {
            return crate::instance::read_setting_at(key);
        }
        crate::instance::read_setting()
    }

    #[cfg(windows)]
    fn write_setting(&self, on: bool) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(key) = &self.setting_subkey {
            return crate::instance::write_setting_at(key, on);
        }
        crate::instance::write_setting(on)
    }

    #[cfg(windows)]
    fn receiving_pipe_name(&self) -> std::io::Result<String> {
        #[cfg(test)]
        if let Some(name) = &self.pipe_name {
            return Ok(name.clone());
        }
        crate::instance::name_for_this_exe()
    }

    /// Opens the file named on the command line; returns whether a document was loaded.
    /// A plain file costs one read, as before: the `.lnk` check is a string compare and a
    /// folder is looked for only after the read failed. On Windows a shortcut opens its
    /// target; a folder opens its README or first Markdown file.
    fn open_command_line_path(&mut self, path: PathBuf, ctx: &Context) -> bool {
        #[cfg(windows)]
        let path = {
            let mut path = path;
            if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
                let _com = crate::win::ComApartment::init();
                match crate::win::shortcut_target(&path) {
                    Some(target) => path = target,
                    None => {
                        self.set_status(format!("{} does not point to a file", links::display_path(&path)));
                        return false;
                    }
                }
            }
            path
        };
        if self.load_path(path.clone(), true, ctx) {
            return true;
        }
        if !path.is_dir() {
            return false;
        }
        match markdown_in_folder(&path) {
            Some(file) => self.load_path(file, true, ctx),
            None => {
                // The failed read above left "Cannot read <folder>: Access is denied" on the
                // screen, which misleads: replace it, as "No input on stdin" does.
                let msg = format!("No Markdown files in {}", links::display_path(&path));
                self.set_status(msg.clone());
                self.error = Some(msg);
                false
            }
        }
    }

    // ------------------------------------------------------------------
    // Loading
    // ------------------------------------------------------------------

    fn set_document(&mut self, text: String, path: Option<PathBuf>, ctx: &Context) {
        let t0 = Instant::now();
        fonts::add_script_fallbacks(ctx, &self.fonts, &text);
        if let Some(p) = &path {
            fonts::add_script_fallbacks(ctx, &self.fonts, &p.to_string_lossy());
        }
        let doc = Document::parse(&text);
        log::debug!("parsed {} bytes into {} blocks in {:.2} ms", text.len(), doc.blocks.len(), t0.elapsed().as_secs_f64() * 1000.0);
        self.gen += 1;
        self.hl.retain_generation(self.gen);
        self.diagrams.retain_generation(self.gen);
        self.measures = vec![Measure::default(); doc.blocks.len()];
        self.block_tops = vec![0.0; doc.blocks.len()];
        self.measure_all = doc.blocks.len() <= 600;
        self.error = None;
        self.source = text;
        let title = match (&doc.title, &path) {
            (_, Some(p)) => format!("{} — mdread", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
            (Some(t), None) => format!("{t} — mdread"),
            (None, None) => "mdread".to_owned(),
        };
        ctx.send_viewport_cmd(ViewportCommand::Title(title));
        self.doc = Some(Arc::new(doc));
        self.path = path;
        self.hovered_link = None;
        if self.search.open {
            self.search.last_gen = 0; // force re-run
        }
    }

    /// Returns whether the file was read. On failure a shown page stays and the status bar
    /// says why; with nothing shown, or for a history move, the error screen shows instead.
    fn load_path(&mut self, path: PathBuf, push_history: bool, ctx: &Context) -> bool {
        self.load_path_in(path, push_history, None, ctx)
    }

    /// Open `path` with the files sidebar showing `files_dir`, or a folder chosen by
    /// `files::sidebar_root` when that is `None`. Returns whether the file was read.
    fn load_path_in(&mut self, path: PathBuf, push_history: bool, files_dir: Option<PathBuf>, ctx: &Context) -> bool {
        // A navigation, failed or not: a slow forward result must not replace what the user asked for since.
        self.bump_nav_gen();
        let (path, read) = read_for_open(&path);
        match read {
            Ok((decoded, stamp)) => {
                self.finish_load(path, decoded, stamp, push_history, files_dir, ctx);
                true
            }
            Err(e) => {
                let msg = format!("Cannot read {}: {e}", links::display_path(&path));
                // An open that fails (Ctrl+O, a drop, a link, a sidebar click) leaves the page
                // alone and says why in the status bar. A history move (`push_history` false)
                // or an empty window still shows the error screen: there is nothing to keep.
                if push_history && self.doc.is_some() {
                    self.set_status(msg);
                    self.pending_anchor = None;
                    return false;
                }
                self.error = Some(msg);
                self.doc = None;
                self.path = Some(path);
                ctx.send_viewport_cmd(ViewportCommand::Title("mdread".into()));
                // An anchor waiting for this document has nothing to jump in.
                self.pending_anchor = None;
                false
            }
        }
    }

    /// Shows a document that was read (by `load_path_in`, or by a forward worker): history,
    /// title, watcher and sidebar. `stamp` is the file's stamp from before the read, so a
    /// change made while it was read still counts as a change.
    fn finish_load(&mut self, path: PathBuf, decoded: text::Decoded, stamp: Option<FileStamp>, push_history: bool, files_dir: Option<PathBuf>, ctx: &Context) {
        if push_history {
            self.save_visit();
            if !self.history.is_empty() {
                self.history.truncate(self.history_pos + 1);
            }
            self.history.push(Visit { path: path.clone(), offset: 0.0, files_dir: None });
            self.history_pos = self.history.len() - 1;
        }
        self.stdin_mode = false;
        self.set_document(decoded.text, Some(path.clone()), ctx);
        if let Some(enc) = decoded.encoding {
            self.set_status(format!("Decoded as {enc}"));
        }
        self.pending_offset = Some(0.0);
        // Also the offset `save_visit` records if another open follows before a frame draws
        // this one (several forwards applied in one frame).
        self.scroll_offset = 0.0;
        self.jump = None;
        self.reload_retries = 0;
        self.setup_watcher(&path, stamp, ctx);
        let home = fonts::home_dir();
        let dir = files_dir.or_else(|| files::sidebar_root(self.files_dir.as_deref(), &path, home.as_deref()));
        self.set_files_dir(dir, ctx);
    }

    fn bump_nav_gen(&mut self) {
        self.nav_gen = take_ticket(&self.open_tickets);
    }

    /// Shows what a forward worker read, as a new history entry. Dropped only if it was
    /// committed before the last navigation on the UI thread; other forwards never make it
    /// stale, so results apply in the order they reach the UI thread. The window was
    /// already restored and focused on the pipe thread; that is not undone when the result
    /// is dropped.
    #[cfg(windows)]
    fn apply_forward(&mut self, r: crate::instance::ForwardResult, ctx: &Context) {
        if r.gen < self.nav_gen {
            log::debug!("forwarded {}: dropped, something else was opened since", r.request.path.display());
            return;
        }
        self.menu = None;
        let opened = match r.outcome {
            Ok(opened) => opened,
            Err(msg) => {
                log::info!("forwarded {}: {msg}", r.request.path.display());
                if self.doc.is_some() {
                    // A shown page stays, as for a failed Ctrl+O.
                    self.set_status(msg);
                } else {
                    // The welcome or error screen: the error screen, for this request. `path`
                    // is cleared so Ctrl+R does not retry an earlier file under this message.
                    self.error = Some(msg);
                    self.path = None;
                    self.pending_anchor = None;
                    ctx.send_viewport_cmd(ViewportCommand::Title("mdread".into()));
                }
                return;
            }
        };
        let path = opened.path.clone();
        if self.doc.is_some() && self.path.as_ref() == Some(&opened.path) {
            // Already shown: no reload and no second history entry; the anchor jumps, or the
            // scroll position stays.
            if let Some(b) = r.request.anchor.as_deref().and_then(|a| self.doc.as_ref()?.find_slug(a)) {
                self.jump_to_block(b);
            }
        } else {
            self.finish_load(opened.path, opened.decoded, opened.stamp, true, None, ctx);
            self.pending_anchor = r.request.anchor.clone();
        }
        if let Some(q) = r.request.find.as_ref().filter(|q| !q.is_empty()) {
            self.search.open = true;
            self.search.query = q.clone();
            self.search.focus_pending = true;
            // Run again even for the same query, so the view goes to the first match.
            self.search.last_gen = 0;
        }
        self.forward_shown = Some((path, r.arrived));
    }

    fn reload(&mut self, ctx: &Context, announce: bool) {
        let Some(path) = self.path.clone() else { return };
        let stamp = file_stamp(&path);
        match std::fs::read(&path) {
            Ok(bytes) => {
                self.reload_retries = 0;
                *self.seen.lock() = stamp;
                let text = text::decode(bytes).text;
                if text == self.source {
                    return;
                }
                let offset = self.scroll_offset;
                self.set_document(text, Some(path), ctx);
                self.pending_offset = Some(offset);
                if announce {
                    self.set_status("Reloaded");
                }
            }
            Err(e) if is_transient_read_error(&e) && self.reload_retries < 4 => {
                self.reload_retries += 1;
                let delay = Duration::from_millis(50 << self.reload_retries);
                log::debug!("reload {}: {e}; retrying in {delay:?}", path.display());
                self.reload_at = Some(Instant::now() + delay);
                wake_after(ctx, delay + Duration::from_millis(5));
            }
            Err(e) => {
                self.reload_retries = 0;
                self.set_status(format!("Reload failed: {e}"));
            }
        }
    }

    /// Watches `path`, last read when it had `stamp`.
    fn setup_watcher(&mut self, path: &Path, stamp: Option<FileStamp>, ctx: &Context) {
        self.watcher = None;
        self.watch_rx = None;
        self.watch_alive.store(false, Ordering::Relaxed);
        // A new cell per watcher: the stopped poller may be past its `alive` check and
        // about to store the old file's stamp.
        self.seen = Arc::new(egui::mutex::Mutex::new(stamp));
        let Some(dir) = path.parent() else { return };
        let (tx, rx) = mpsc::channel();
        self.watch_rx = Some(rx);
        #[cfg(windows)]
        self.spawn_stamp_poller(path, tx.clone(), ctx);
        let target = path.to_path_buf();
        let ctx = ctx.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => {
                let relevant = ev.paths.is_empty() || ev.paths.iter().any(|p| is_watched_file(p, &target));
                if relevant && tx.send(WatchEvent::Changed).is_ok() {
                    ctx.request_repaint();
                }
            }
            Err(e) if cfg!(windows) => {
                log::warn!("file watcher: {e}");
                if tx.send(WatchEvent::Missed).is_ok() {
                    ctx.request_repaint();
                }
            }
            Err(_) => {}
        });
        match watcher {
            Ok(mut w) => {
                if let Err(e) = w.watch(dir, notify::RecursiveMode::NonRecursive) {
                    log::warn!("watch {}: {e}", dir.display());
                } else {
                    self.watcher = Some(w);
                }
            }
            Err(e) => log::warn!("file watcher unavailable: {e}"),
        }
    }

    /// Checks the file's size and modification time every few seconds. Catches what
    /// ReadDirectoryChangesW misses: notify stops watching without a word after an error
    /// or a buffer overflow, and changes made on network shares and in WSL often never
    /// arrive. Only stat calls; nothing is read or repainted unless the file changed.
    #[cfg(windows)]
    fn spawn_stamp_poller(&mut self, path: &Path, tx: mpsc::Sender<WatchEvent>, ctx: &Context) {
        let alive = Arc::new(AtomicBool::new(true));
        self.watch_alive = alive.clone();
        let seen = self.seen.clone();
        let path = path.to_path_buf();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("file-poll".into())
            .spawn(move || loop {
                std::thread::sleep(STAMP_POLL);
                if !alive.load(Ordering::Relaxed) {
                    return;
                }
                let now = file_stamp(&path);
                let mut seen = seen.lock();
                if *seen != now {
                    *seen = now;
                    if tx.send(WatchEvent::Missed).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .ok();
    }

    fn open_dialog(&mut self, ctx: &Context, frame: &eframe::Frame) {
        if self.dialog_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let start_dir = self.path.as_deref().and_then(Path::parent).map(Path::to_path_buf).or_else(|| self.files_dir.clone());
        // The native Open dialog, owned by the window so it stays on top of it and keeps it
        // disabled while open. It runs on the helper thread: its modal loop would stall
        // the event loop here.
        #[cfg(windows)]
        let dialog = {
            let dialog = rfd::FileDialog::new()
                .set_title("Open Markdown file")
                .add_filter("Markdown", &["md", "markdown", "mdown", "mkd", "mdx", "txt"])
                .add_filter("All files", &["*"])
                .set_parent(frame);
            match &start_dir {
                Some(d) => dialog.set_directory(dunce::simplified(d)),
                None => dialog,
            }
        };
        #[cfg(not(windows))]
        let _ = frame;
        std::thread::Builder::new()
            .name("file-dialog".into())
            .spawn(move || {
                #[cfg(windows)]
                let path = dialog.pick_file();
                #[cfg(not(windows))]
                let path = file_dialog_command(start_dir.as_deref())
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                let _ = tx.send(path);
                ctx.request_repaint();
            })
            .ok();
        self.dialog_rx = Some(rx);
    }

    /// Opens a file dropped on the window. A dropped folder opens its README or first
    /// Markdown file (reading a folder as a file fails on Windows with a puzzling "Access
    /// is denied"), and on Windows a dropped shortcut opens its target.
    fn open_dropped(&mut self, path: PathBuf, ctx: &Context) {
        #[cfg(windows)]
        let path = {
            let mut path = path;
            if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
                let _com = crate::win::ComApartment::init();
                match crate::win::shortcut_target(&path) {
                    Some(target) => path = target,
                    None => {
                        self.set_status(format!("{} does not point to a file", links::display_path(&path)));
                        return;
                    }
                }
            }
            path
        };
        let path = if path.is_dir() {
            match markdown_in_folder(&path) {
                Some(file) => file,
                None => {
                    self.set_status(format!("No Markdown files in {}", links::display_path(&path)));
                    return;
                }
            }
        } else {
            path
        };
        self.load_path(path, true, ctx);
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    /// Fonts for East Asian scripts and symbols in the UI's own short texts (messages and
    /// the search box), which can need them before any document does.
    fn add_ui_script_fallbacks(&self, ctx: &Context) {
        let status = self.status.as_ref().map(|(s, _)| s.as_str());
        let query = self.search.open.then_some(self.search.query.as_str());
        for text in [status, self.error.as_deref(), query].into_iter().flatten() {
            fonts::add_script_fallbacks(ctx, &self.fonts, text);
        }
    }

    // ------------------------------------------------------------------
    // Navigation
    // ------------------------------------------------------------------

    fn jump_to_block(&mut self, block: usize) {
        self.jump = Some((block, 6));
        self.jump_until = Instant::now() + Duration::from_secs(2);
        self.pending_offset = None;
    }

    fn go_history(&mut self, delta: i32, ctx: &Context) {
        let Some(visit) = self.history_pos.checked_add_signed(delta as isize).and_then(|i| self.history.get(i)).cloned() else {
            return;
        };
        self.save_visit();
        self.history_pos = (self.history_pos as isize + delta as isize) as usize;
        self.load_path_in(visit.path, false, visit.files_dir, ctx);
        self.pending_offset = Some(visit.offset);
        self.scroll_offset = visit.offset;
    }

    /// Note where the current file was left, for coming back to it.
    fn save_visit(&mut self) {
        if let Some(cur) = self.history.get_mut(self.history_pos) {
            cur.offset = self.scroll_offset;
            cur.files_dir = self.files_dir.clone();
        }
    }

    /// The file `delta` steps back (-1) or forward (1) in the history.
    fn history_neighbour(&self, delta: isize) -> Option<&Path> {
        let i = self.history_pos.checked_add_signed(delta)?;
        self.history.get(i).map(|v| v.path.as_path())
    }

    /// Show `dir` in the files sidebar, scanning it if it changed.
    fn set_files_dir(&mut self, dir: Option<PathBuf>, ctx: &Context) {
        if dir == self.files_dir {
            return;
        }
        self.files_dir = dir.clone();
        self.files.clear();
        self.files_rx = dir.map(|d| files::spawn_scan(d, ctx.clone()));
    }

    fn open_link(&mut self, url: &str, ctx: &Context) {
        let base = self.link_base();
        match links::classify_link(url, base.as_deref()) {
            None => {}
            Some(Target::Anchor(anchor)) => {
                let target = self.doc.as_ref().and_then(|d| d.find_slug(&anchor));
                match target {
                    Some(b) => self.jump_to_block(b),
                    None => self.set_status(format!("Anchor not found: #{anchor}")),
                }
            }
            Some(Target::External(url)) => {
                self.launch(url.clone(), false, ctx);
                self.set_status(format!("Opened {url}"));
            }
            Some(Target::Blocked(msg)) => self.set_status(msg),
            Some(Target::Local(abs, frag)) => self.open_local(abs, frag, ctx),
        }
    }

    /// The folder relative links start from: the document's, or with stdin the current
    /// folder, which a terminal set (stdin is only ever piped from one).
    fn link_base(&self) -> Option<PathBuf> {
        if self.stdin_mode {
            return std::env::current_dir().ok();
        }
        self.path.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf)
    }

    fn open_local(&mut self, abs: PathBuf, frag: Option<String>, ctx: &Context) {
        // Resolve `..` and links first, so the type check below sees the real file.
        #[cfg(windows)]
        let abs = dunce::canonicalize(&abs).unwrap_or(abs);
        if abs.is_dir() {
            for name in FOLDER_INDEX {
                let p = abs.join(name);
                if p.is_file() {
                    if self.load_path(p, true, ctx) {
                        self.pending_anchor = frag;
                    }
                    return;
                }
            }
            self.launch(abs, false, ctx);
            return;
        }
        if abs.is_file() {
            if files::is_markdown(&abs) || abs.extension().is_none() {
                if self.load_path(abs, true, ctx) {
                    self.pending_anchor = frag;
                }
            } else if links::opens_in_default_app(&abs) {
                self.set_status(format!("Opened {}", links::display_path(&abs)));
                self.launch(abs, false, ctx);
            } else {
                // Could be a program or script: show it instead of running it.
                let name = abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.set_status(format!("Showing {name} in Explorer"));
                self.launch(abs, true, ctx);
            }
        } else {
            self.set_status(format!("Not found: {}", links::display_path(&abs)));
        }
    }

    /// The link as it should read outside the viewer: web links and anchors as
    /// written, relative file links as absolute paths.
    fn link_for_clipboard(&self, url: &str) -> String {
        match links::classify_link(url, self.link_base().as_deref()) {
            Some(Target::Local(abs, frag)) => {
                let abs = links::display_path(&abs);
                match frag {
                    Some(f) if !f.is_empty() => format!("{abs}#{f}"),
                    _ => abs,
                }
            }
            _ => url.trim().to_owned(),
        }
    }

    // ------------------------------------------------------------------
    // Context menu
    // ------------------------------------------------------------------

    /// Returns false for events egui should not see; see `raw_input_hook`.
    fn keep_event(&mut self, e: &Event, copy: &mut bool) -> bool {
        match e {
            Event::PointerButton { pos, button, pressed: true, .. } => {
                let (pos, button) = (*pos, *button);
                if let Some(menu) = self.menu.take() {
                    if menu.rect.contains(pos) {
                        if menu.copy_rect.is_some_and(|r| r.contains(pos)) {
                            *copy = true;
                            self.swallow_release = Some(button);
                            return false;
                        }
                        self.menu = Some(menu);
                        return true;
                    }
                    if button != PointerButton::Secondary {
                        // A click outside only dismisses the menu.
                        self.swallow_release = Some(button);
                        return false;
                    }
                }
                if button == PointerButton::Secondary && !self.show_help && self.content_rect.contains(pos) {
                    log::trace!("context menu requested at {pos:?}");
                    self.menu_request = Some(pos);
                    self.swallow_release = Some(button);
                    return false;
                }
                true
            }
            Event::PointerButton { button, pressed: false, .. } => {
                if self.swallow_release == Some(*button) {
                    self.swallow_release = None;
                    return false;
                }
                true
            }
            Event::MouseWheel { .. } => {
                self.menu = None;
                true
            }
            _ => true,
        }
    }

    fn show_context_menu(&mut self, ctx: &Context) {
        let Some(mut menu) = self.menu.take() else { return };
        let has_selection = ctx.plugin::<LabelSelectionState>().lock().has_selection();
        let base = self.settings.base_size;
        let mut picked: Option<MenuItem> = None;
        let mut copy_rect = None;
        let out = Popup::new(Id::unique("context-menu"), ctx.clone(), PopupAnchor::Position(menu.pos), LayerId::background())
            .kind(PopupKind::Menu)
            .open(true)
            .close_behavior(PopupCloseBehavior::IgnoreClicks)
            .layout(Layout::top_down_justified(Align::Min))
            .style(egui::containers::menu::menu_style)
            .show(|ui| {
                ui.set_min_width(base * 9.0);
                // A press on "Copy" is handled in `raw_input_hook`, see `ContextMenu::copy_rect`.
                let copy = ui.add_enabled(has_selection, Button::new("Copy"));
                if has_selection {
                    copy_rect = Some(copy.rect);
                }
                if menu.link.is_some() {
                    ui.separator();
                    if ui.button("Open link").clicked() {
                        picked = Some(MenuItem::OpenLink);
                    }
                    if ui.button("Copy link").clicked() {
                        picked = Some(MenuItem::CopyLink);
                    }
                }
                if menu.image.is_some() {
                    ui.separator();
                    if ui.button("Open image").clicked() {
                        picked = Some(MenuItem::OpenImage);
                    }
                    if ui.button("Copy image address").clicked() {
                        picked = Some(MenuItem::CopyImage);
                    }
                }
                if menu.code.is_some() {
                    ui.separator();
                    if ui.button("Copy code block").clicked() {
                        picked = Some(MenuItem::CopyCode);
                    }
                }
            });
        menu.rect = out.map(|o| o.response.rect).unwrap_or(Rect::NOTHING);
        menu.copy_rect = copy_rect;
        match picked {
            None => self.menu = Some(menu),
            Some(item) => self.menu_action(item, &menu, ctx),
        }
    }

    fn menu_action(&mut self, item: MenuItem, menu: &ContextMenu, ctx: &Context) {
        match item {
            MenuItem::OpenLink => {
                if let Some(url) = &menu.link {
                    self.open_link(url, ctx);
                }
            }
            MenuItem::CopyLink => {
                if let Some(url) = &menu.link {
                    ctx.copy_text(self.link_for_clipboard(url));
                    self.set_status("Copied link to clipboard");
                }
            }
            MenuItem::OpenImage => {
                if let Some(uri) = &menu.image {
                    // Windows hands the file to the shell, so it gets the checks a click on a
                    // link gets: only safe types open, the rest is revealed in Explorer.
                    #[cfg(windows)]
                    match links::file_uri_to_path(uri) {
                        Some(path) => self.open_local(path, None, ctx),
                        None => self.open_link(uri, ctx),
                    }
                    #[cfg(not(windows))]
                    {
                        let target = local_path(uri);
                        self.launch(target, false, ctx);
                        self.set_status(format!("Opened {target}"));
                    }
                }
            }
            MenuItem::CopyImage => {
                if let Some(uri) = &menu.image {
                    ctx.copy_text(image_address(uri));
                    self.set_status("Copied image address to clipboard");
                }
            }
            MenuItem::CopyCode => {
                if let Some(code) = &menu.code {
                    ctx.copy_text(code.to_string());
                    self.set_status("Copied code block to clipboard");
                }
            }
        }
    }

    /// Opens a URL or path with the OS (or, with `reveal`, shows the file in Explorer) on a
    /// helper thread, as the shell can block. Failures come back to the status bar.
    fn launch(&self, target: impl Into<OsString>, reveal: bool, ctx: &Context) {
        let target: OsString = target.into();
        let tx = self.launch_tx.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("open".into())
            .spawn(move || {
                #[cfg(windows)]
                let result = {
                    let _com = crate::win::ComApartment::init();
                    if reveal {
                        crate::win::reveal_in_explorer(Path::new(&target))
                    } else {
                        open::that_detached(&target)
                    }
                };
                #[cfg(not(windows))]
                let result = {
                    let _ = reveal;
                    open::that_detached(&target)
                };
                if let Err(e) = result {
                    let label = links::display_path(Path::new(&target));
                    log::warn!("open {label}: {e}");
                    if tx.send(format!("Could not open {label}: {e}")).is_ok() {
                        ctx.request_repaint();
                    }
                }
            })
            .ok();
    }

    // ------------------------------------------------------------------
    // Appearance
    // ------------------------------------------------------------------

    fn apply_theme(&mut self, ctx: &Context) {
        let dark = match self.settings.theme {
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
            ThemeChoice::System => self.system_dark.unwrap_or(true),
        };
        self.pal = if dark { Palette::dark() } else { Palette::light() };
        theme::apply(ctx, &self.pal, &self.fonts, self.settings.base_size);
    }

    fn toggle_theme(&mut self, ctx: &Context) {
        self.settings.theme = if self.pal.dark { ThemeChoice::Light } else { ThemeChoice::Dark };
        // An explicit toggle is a real preference; a CLI override was not.
        self.theme_override = None;
        self.apply_theme(ctx);
    }

    /// Gives the Windows title bar the app's theme, which can differ from the system's.
    #[cfg(windows)]
    fn sync_title_bar(&mut self, frame: &eframe::Frame) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if self.title_bar_dark == Some(self.pal.dark) {
            return;
        }
        if let Ok(RawWindowHandle::Win32(w)) = frame.window_handle().map(|h| h.as_raw()) {
            crate::win::set_title_bar_dark(w.hwnd.get(), self.pal.dark);
            self.title_bar_dark = Some(self.pal.dark);
        }
    }

    fn zoom_to(&mut self, ctx: &Context, zoom: f32) {
        let zoom = zoom.clamp(0.5, 3.0);
        ctx.set_zoom_factor(zoom);
        self.settings.zoom = zoom;
    }

    /// The reading column's width as drawn, in points.
    fn column_now(&self) -> f32 {
        if self.settings.full_width {
            self.column_max
        } else {
            self.settings.column_width.min(self.column_max)
        }
    }

    fn set_column_width(&mut self, w: f32) {
        self.settings.full_width = false;
        self.settings.column_width = w.clamp(MIN_COLUMN, MAX_COLUMN);
    }

    fn width_step(&self, wider: bool) -> Option<WidthStep> {
        width_step(self.column_now(), self.settings.full_width, self.column_max, wider)
    }

    fn apply_width_step(&mut self, step: WidthStep) {
        match step {
            WidthStep::Preset(w) => self.set_column_width(w),
            WidthStep::Full => self.settings.full_width = true,
        }
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    fn handle_input(&mut self, ctx: &Context, frame: &eframe::Frame) {
        // Dropped files
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if let Some(p) = dropped.into_iter().next() {
            self.open_dropped(p, ctx);
        }

        let menu_open = self.menu.is_some();
        if menu_open && ctx.input(|i| i.events.iter().any(|e| matches!(e, Event::Key { pressed: true, .. }))) {
            self.menu = None;
        }

        let typing = ctx.memory(|m| m.focused().is_some());
        let line = self.settings.base_size * 1.55 * 3.0;
        let page = (self.viewport_h * 0.88).max(line);
        let mut delta = 0.0;
        let mut zoom: Option<f32> = None;
        let cur_zoom = ctx.zoom_factor();

        let mut cmds: Vec<Cmd> = Vec::new();
        ctx.input_mut(|i| {
            let zd = i.zoom_delta();
            if (zd - 1.0).abs() > 1e-4 {
                zoom = Some(cur_zoom * zd);
            }
            cmds = key_commands(i, typing);
            // The side buttons on the mouse, as in file managers and browsers.
            if i.pointer.button_pressed(egui::PointerButton::Extra1) {
                cmds.push(Cmd::Back);
            }
            if i.pointer.button_pressed(egui::PointerButton::Extra2) {
                cmds.push(Cmd::Forward);
            }
        });

        for c in &cmds {
            match *c {
                Cmd::Lines(n) => delta += line * n,
                Cmd::Pages(n) => delta += page * n,
                Cmd::ZoomIn => zoom = Some(cur_zoom * 1.1),
                Cmd::ZoomOut => zoom = Some(cur_zoom / 1.1),
                _ => {}
            }
        }
        if delta != 0.0 {
            self.scroll_delta += delta;
        }
        if let Some(z) = zoom {
            self.zoom_to(ctx, z);
        }
        for c in cmds {
            match c {
                Cmd::Lines(_) | Cmd::Pages(_) | Cmd::ZoomIn | Cmd::ZoomOut => {}
                Cmd::Open => self.open_dialog(ctx, frame),
                Cmd::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
                Cmd::Search => {
                    self.search.open = true;
                    self.search.focus_pending = true;
                }
                Cmd::Reload => self.reload(ctx, true),
                Cmd::Back => self.go_history(-1, ctx),
                Cmd::Forward => self.go_history(1, ctx),
                Cmd::Escape => {
                    if menu_open {
                        // Already closed above.
                    } else if self.show_help {
                        self.show_help = false;
                    } else if self.search.open {
                        self.search.clear();
                    }
                }
                Cmd::Top => {
                    self.pending_offset = Some(0.0);
                    self.jump = None;
                }
                Cmd::Bottom => {
                    self.pending_offset = Some(f32::MAX);
                    self.jump = None;
                }
                Cmd::Next => {
                    if self.search.open {
                        self.search.next();
                        if let Some(b) = self.search.current_block() {
                            self.jump_to_block(b);
                        }
                    }
                }
                Cmd::Prev => {
                    if self.search.open {
                        self.search.prev();
                        if let Some(b) = self.search.current_block() {
                            self.jump_to_block(b);
                        }
                    }
                }
                Cmd::Outline => self.settings.show_outline = !self.settings.show_outline,
                Cmd::Files => self.settings.show_files = !self.settings.show_files,
                Cmd::Theme => self.toggle_theme(ctx),
                Cmd::Width => self.settings.full_width = !self.settings.full_width,
                // From the width as drawn, so the first press out of full width (or a
                // column the window caps) narrows it.
                Cmd::Narrower => self.set_column_width(self.column_now() - 60.0),
                Cmd::Wider => {
                    if !self.settings.full_width {
                        self.set_column_width(self.settings.column_width + 60.0);
                    }
                }
                Cmd::Help => {
                    self.show_help = !self.show_help;
                    // Read once per opening, not per frame, so it shows a change made in
                    // another window.
                    #[cfg(windows)]
                    if self.show_help {
                        self.setting_on = self.read_setting();
                    }
                }
                Cmd::ZoomReset => self.zoom_to(ctx, 1.0),
            }
        }
    }

    fn poll_channels(&mut self, ctx: &Context) {
        if let Some(rx) = &self.watch_rx {
            let (mut changed, mut missed) = (false, false);
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    WatchEvent::Changed => changed = true,
                    WatchEvent::Missed => missed = true,
                }
            }
            if missed {
                if let Some(path) = self.path.clone() {
                    log::debug!("{} changed without a watcher event; watching again", path.display());
                    let stamp = *self.seen.lock();
                    self.setup_watcher(&path, stamp, ctx);
                }
            }
            if changed || missed {
                let at = Instant::now() + Duration::from_millis(60);
                self.reload_at = Some(at);
                wake_after(ctx, Duration::from_millis(70));
            }
        }
        if let Some(t) = self.reload_at {
            if Instant::now() >= t {
                self.reload_at = None;
                self.reload(ctx, true);
            } else {
                wake_after(ctx, t - Instant::now() + Duration::from_millis(5));
            }
        }
        if self.hl.poll() {
            ctx.request_repaint();
        }
        self.diagrams.poll();
        while let Ok(msg) = self.launch_rx.try_recv() {
            self.set_status(msg);
        }
        #[cfg(windows)]
        while let Ok(r) = self.forward_rx.try_recv() {
            self.apply_forward(r, ctx);
        }
        if let Some(rx) = &self.dialog_rx {
            match rx.try_recv() {
                Ok(Some(p)) => {
                    self.dialog_rx = None;
                    self.load_path(p, true, ctx);
                }
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => self.dialog_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.files_rx {
            match rx.try_recv() {
                Ok(files) => {
                    for f in &files {
                        fonts::add_script_fallbacks(ctx, &self.fonts, &f.to_string_lossy());
                    }
                    self.files = files;
                    self.files_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.files_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.settings.theme == ThemeChoice::System {
            let sys = ctx.system_theme().map(|t| t == egui::Theme::Dark);
            if sys.is_some() && sys != self.system_dark {
                self.system_dark = sys;
                self.apply_theme(ctx);
            }
        }
        // Keep zoom in sync when egui handled Ctrl+/- itself.
        let z = ctx.zoom_factor();
        if (z - self.settings.zoom).abs() > 1e-4 {
            self.settings.zoom = z;
        }
        if let Some((_, t)) = &self.status {
            if t.elapsed() > Duration::from_secs(4) {
                self.status = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(250));
                wake_after(ctx, Duration::from_millis(300));
            }
        }
    }

    // ------------------------------------------------------------------
    // Panels
    // ------------------------------------------------------------------

    fn panel_frame(&self) -> Frame {
        Frame::new().fill(self.pal.panel_bg).inner_margin(Margin::symmetric(12, 12))
    }

    fn show_outline(&mut self, ui: &mut Ui) {
        let Some(doc) = self.doc.clone() else { return };
        let headings: Vec<(usize, &crate::document::HeadingRef)> =
            doc.headings.iter().enumerate().filter(|(_, h)| !h.nested).collect();
        if headings.is_empty() {
            return;
        }
        let min_level = headings.iter().map(|(_, h)| h.level).min().unwrap_or(1);
        let base = self.settings.base_size;
        let pal = self.pal;
        Panel::left("outline")
            .resizable(true)
            .default_size(280.0)
            .size_range(160.0..=700.0)
            .frame(self.panel_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("OUTLINE").size(base * 0.72).color(pal.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(format!("{}", headings.len())).size(base * 0.72).color(pal.text_dim));
                    });
                });
                ui.add_space(6.0);
                let mut clicked: Option<usize> = None;
                ScrollArea::vertical().id_salt("outline-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for (hi, h) in &headings {
                        let depth = h.level.saturating_sub(min_level) as f32;
                        let indent = depth * base * 0.85;
                        let is_current = self.current_heading == Some(*hi);
                        let size = match h.level.saturating_sub(min_level) {
                            0 => base * 0.95,
                            1 => base * 0.9,
                            _ => base * 0.85,
                        };
                        let color = if is_current {
                            pal.heading
                        } else if depth <= 1.0 {
                            pal.text
                        } else {
                            pal.text_dim
                        };
                        let mut text = RichText::new(&h.text).size(size).color(color);
                        if is_current || depth == 0.0 {
                            text = text.strong();
                        }
                        let bg_idx = ui.painter().add(Shape::Noop);
                        let avail = ui.available_rect_before_wrap();
                        let row = ui
                            .scope_builder(
                                UiBuilder::new().max_rect(Rect::from_min_max(
                                    pos2(avail.left() + indent + 8.0, avail.top() + 3.0),
                                    pos2(avail.right() - 4.0, avail.bottom()),
                                )),
                                |ui| ui.add(Label::new(text).sense(Sense::click()).selectable(false).truncate()),
                            )
                            .inner;
                        let row_rect = Rect::from_min_max(pos2(avail.left(), row.rect.top() - 3.0), pos2(avail.right(), row.rect.bottom() + 3.0));
                        ui.allocate_rect(Rect::from_min_max(pos2(avail.left(), avail.top()), pos2(avail.right(), row.rect.bottom() + 3.0)), Sense::hover());
                        if is_current {
                            ui.painter().set(bg_idx, Shape::rect_filled(row_rect, CornerRadius::same(4), pal.sidebar_current));
                            ui.painter().rect_filled(Rect::from_min_size(row_rect.left_top(), vec2(3.0, row_rect.height())), CornerRadius::same(2), pal.accent);
                        } else if row.hovered() {
                            ui.painter().set(bg_idx, Shape::rect_filled(row_rect, CornerRadius::same(4), pal.table_stripe));
                        }
                        if row.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if row.clicked() {
                            clicked = Some(h.block);
                        }
                        if self.outline_scroll_to == Some(*hi) {
                            row.scroll_to_me(Some(Align::Center));
                        }
                    }
                });
                self.outline_scroll_to = None;
                if let Some(b) = clicked {
                    self.jump_to_block(b);
                }
            });
    }

    fn show_files_panel(&mut self, ui: &mut Ui, ctx: &Context) {
        let Some(dir) = self.files_dir.clone() else { return };
        let open_dir = self.path.as_deref().and_then(Path::parent).map(Path::to_path_buf);
        // A lone file has no use for the sidebar, unless a link or the sidebar led
        // somewhere and it shows the way back.
        if self.files.len() < 2 && self.history.len() < 2 && open_dir.as_ref() == Some(&dir) {
            return;
        }
        let base = self.settings.base_size;
        let pal = self.pal;
        let current = self.path.clone();
        let mut clicked: Option<PathBuf> = None;
        let mut show_dir: Option<PathBuf> = None;
        Panel::right("files")
            .resizable(true)
            .default_size(260.0)
            .size_range(160.0..=700.0)
            .frame(self.panel_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("FILES").size(base * 0.72).color(pal.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let up = dir.parent();
                        let up_hint = up.map(|u| format!("Show the folder above: {}", folder_name(u))).unwrap_or_default();
                        if ui.add_enabled(up.is_some(), flat_button(arrow("↑", base * 0.78, pal.text_dim))).on_hover_text(up_hint).clicked() {
                            show_dir = up.map(Path::to_path_buf);
                        }
                        // The folder shown, with a menu of the open file's folder and those
                        // above it, to go back down after going up.
                        let resp = ui
                            .add(flat_button(RichText::new(folder_name(&dir)).size(base * 0.72).color(pal.text_dim)).truncate())
                            .on_hover_text(format!("{}\nClick to show another folder", links::display_path(&dir)));
                        Popup::menu(&resp).show(|ui| {
                            for d in open_dir.iter().flat_map(|d| d.ancestors()) {
                                let here = Some(d) == open_dir.as_deref();
                                let item = Button::selectable(d == dir, folder_name(d)).right_text(if here { "open file" } else { "" });
                                if ui.add(item).on_hover_text(links::display_path(d)).clicked() {
                                    show_dir = Some(d.to_path_buf());
                                }
                            }
                        });
                    });
                });
                ui.add_space(6.0);
                ScrollArea::vertical().id_salt("files-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for f in &self.files {
                        let rel = f.strip_prefix(&dir).unwrap_or(f);
                        let is_current = current.as_ref() == Some(f);
                        let depth = rel.components().count().saturating_sub(1) as f32;
                        let label = rel.to_string_lossy().to_string();
                        let color = if is_current { pal.heading } else if depth == 0.0 { pal.text } else { pal.text_dim };
                        let mut text = RichText::new(label).size(base * 0.88).color(color);
                        if is_current {
                            text = text.strong();
                        }
                        let bg_idx = ui.painter().add(Shape::Noop);
                        let avail = ui.available_rect_before_wrap();
                        let row = ui
                            .scope_builder(
                                UiBuilder::new().max_rect(Rect::from_min_max(pos2(avail.left() + 8.0, avail.top() + 3.0), pos2(avail.right() - 4.0, avail.bottom()))),
                                |ui| ui.add(Label::new(text).sense(Sense::click()).selectable(false).truncate()),
                            )
                            .inner;
                        let row_rect = Rect::from_min_max(pos2(avail.left(), row.rect.top() - 3.0), pos2(avail.right(), row.rect.bottom() + 3.0));
                        ui.allocate_rect(Rect::from_min_max(pos2(avail.left(), avail.top()), pos2(avail.right(), row.rect.bottom() + 3.0)), Sense::hover());
                        if is_current {
                            ui.painter().set(bg_idx, Shape::rect_filled(row_rect, CornerRadius::same(4), pal.sidebar_current));
                        } else if row.hovered() {
                            ui.painter().set(bg_idx, Shape::rect_filled(row_rect, CornerRadius::same(4), pal.table_stripe));
                        }
                        if row.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if row.clicked() && !is_current {
                            clicked = Some(f.clone());
                        }
                    }
                });
            });
        if let Some(d) = show_dir {
            self.set_files_dir(Some(d), ctx);
        }
        if let Some(p) = clicked {
            self.load_path(p, true, ctx);
        }
    }

    fn show_search_bar(&mut self, ui: &mut Ui, ctx: &Context) {
        let base = self.settings.base_size;
        let pal = self.pal;
        let mut do_next = false;
        let mut do_prev = false;
        let mut close = false;
        Panel::top("search")
            .resizable(false)
            .frame(Frame::new().fill(pal.panel_bg).inner_margin(Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Find").color(pal.text_dim));
                    let out = TextEdit::singleline(&mut self.search.query)
                        .hint_text("Search this document…")
                        .desired_width((base * 24.0).min(ui.available_width() * 0.5))
                        .show(ui);
                    let resp = out.response;
                    if self.search.focus_pending {
                        resp.request_focus();
                        // Select the previous query so that typing replaces it.
                        let mut state = out.state;
                        let end = CCursor::new(self.search.query.chars().count());
                        state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), end)));
                        state.store(ctx, resp.id);
                        self.search.focus_pending = false;
                    }
                    if resp.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter)) {
                        if ctx.input(|i| i.modifiers.shift) {
                            do_prev = true;
                        } else {
                            do_next = true;
                        }
                        resp.request_focus();
                    }
                    let n = self.search.matches.len();
                    let info = if self.search.query.trim().is_empty() {
                        String::new()
                    } else if n == 0 {
                        "No matches".to_owned()
                    } else {
                        format!("{} of {}", self.search.current + 1, n)
                    };
                    ui.label(RichText::new(info).color(pal.text_dim));
                    if ui.add_enabled(n > 0, egui::Button::new("Prev")).on_hover_text("Shift+Enter or N").clicked() {
                        do_prev = true;
                    }
                    if ui.add_enabled(n > 0, egui::Button::new("Next")).on_hover_text("Enter or n").clicked() {
                        do_next = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Close").on_hover_text("Esc").clicked() {
                            close = true;
                        }
                    });
                });
            });
        if close {
            self.search.clear();
        }
        if do_next {
            self.search.next();
            if let Some(b) = self.search.current_block() {
                self.jump_to_block(b);
            }
        }
        if do_prev {
            self.search.prev();
            if let Some(b) = self.search.current_block() {
                self.jump_to_block(b);
            }
        }
    }

    fn show_status_bar(&mut self, ui: &mut Ui) {
        let base = self.settings.base_size;
        let pal = self.pal;
        let small = base * 0.78;
        let mut nav = None;
        Panel::bottom("status")
            .resizable(false)
            .show_separator_line(true)
            .frame(Frame::new().fill(pal.panel_bg).inner_margin(Margin::symmetric(12, 5)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.history.len() > 1 {
                        nav = self.history_buttons(ui, small);
                        ui.add_space(6.0);
                    }
                    let path_text = if self.stdin_mode {
                        "stdin".to_owned()
                    } else {
                        self.path.as_deref().map(links::display_path).unwrap_or_default()
                    };
                    ui.add(Label::new(RichText::new(path_text).size(small).color(pal.text_dim)).truncate());
                    if let Some((msg, _)) = &self.status {
                        ui.add_space(12.0);
                        ui.label(RichText::new(msg).size(small).color(pal.accent));
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let zoom = (self.settings.zoom * 100.0).round() as i32;
                        let pct = if self.content_h > self.viewport_h + 1.0 {
                            (self.scroll_offset / (self.content_h - self.viewport_h) * 100.0).clamp(0.0, 100.0).round() as i32
                        } else {
                            100
                        };
                        ui.label(RichText::new(format!("{zoom}%")).size(small).color(pal.text_dim)).on_hover_text(format!("Zoom ({CMD} +/−/0, {CMD}+scroll)"));
                        ui.separator();
                        ui.label(RichText::new(format!("{pct}%")).size(small).color(pal.text_dim)).on_hover_text("Scroll position");
                        if let Some(doc) = &self.doc {
                            ui.separator();
                            let words = doc.word_count;
                            let mins = (words as f32 / 230.0).ceil().max(1.0) as usize;
                            ui.label(RichText::new(format!("{words} words · {mins} min")).size(small).color(pal.text_dim));
                        }
                        ui.separator();
                        self.width_controls(ui, small);
                        ui.separator();
                        ui.label(RichText::new("? help").size(small).color(pal.text_dim));
                    });
                });
            });
        if let Some(delta) = nav {
            let ctx = ui.ctx().clone();
            self.go_history(delta, &ctx);
        }
    }

    /// ← → in the status bar, for going back to the file a link came from. Returns the
    /// step clicked.
    fn history_buttons(&self, ui: &mut Ui, size: f32) -> Option<i32> {
        let mut nav = None;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (delta, glyph, verb, keys) in [(-1, "←", "Back", "Alt+Left, Backspace or the mouse back button"), (1, "→", "Forward", "Alt+Right")] {
                let to = self.history_neighbour(delta);
                let resp = ui.add_enabled(to.is_some(), flat_button(arrow(glyph, size, self.pal.text_dim)));
                if let Some(to) = to {
                    // The folder too: links often lead to another README.md.
                    let shown = match to.parent() {
                        Some(dir) => format!("{}/{}", folder_name(dir), to.file_name().unwrap_or_default().to_string_lossy()),
                        None => links::display_path(to),
                    };
                    if resp.on_hover_text(format!("{verb} to {shown} ({keys})")).clicked() {
                        nav = Some(delta as i32);
                    }
                }
            }
        });
        nav
    }

    /// `− 860pt +` in the status bar, which runs right to left: the buttons step through
    /// the width presets, and the width opens a menu of them.
    fn width_controls(&mut self, ui: &mut Ui, size: f32) {
        let pal = self.pal;
        let flat = |text: &str| flat_button(RichText::new(text).size(size).color(pal.text_dim));
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let wider = self.width_step(true);
            if ui.add_enabled(wider.is_some(), flat("+")).on_hover_text("Wider (] in small steps)").clicked() {
                if let Some(step) = wider {
                    self.apply_width_step(step);
                }
            }
            let label = if self.settings.full_width { "full width".to_owned() } else { format!("{:.0}pt", self.column_now()) };
            // Wide enough for "full width", so the − button stays put while stepping.
            let full_w = ui.painter().layout_no_wrap("full width".to_owned(), egui::FontId::proportional(size), pal.text_dim).size().x;
            let min_w = full_w + 2.0 * ui.spacing().button_padding.x;
            let resp = ui
                .add(flat(&label).min_size(vec2(min_w, 0.0)))
                .on_hover_text("Reading column width: click for presets (w toggles full width)");
            Popup::menu(&resp).show(|ui| {
                for (name, w) in WIDTH_PRESETS {
                    let on = !self.settings.full_width && (self.settings.column_width - w).abs() < 0.5;
                    let fits = w <= self.column_max + 1.0;
                    let item = Button::selectable(on, name).right_text(format!("{w:.0}pt"));
                    let resp = ui.add_enabled(fits || on, item).on_disabled_hover_text("Wider than the window");
                    if resp.clicked() {
                        self.set_column_width(w);
                    }
                }
                ui.separator();
                if ui.add(Button::selectable(self.settings.full_width, "Full width").right_text("w")).clicked() {
                    self.settings.full_width = true;
                }
            });
            let narrower = self.width_step(false);
            if ui.add_enabled(narrower.is_some(), flat("−")).on_hover_text("Narrower ([ in small steps)").clicked() {
                if let Some(step) = narrower {
                    self.apply_width_step(step);
                }
            }
        });
    }

    fn show_help(&mut self, ctx: &Context) {
        if !self.show_help {
            return;
        }
        let pal = self.pal;
        let base = self.settings.base_size;
        let mut open = self.show_help;
        // The checkbox's click, applied once the window is drawn.
        #[cfg(windows)]
        let mut clicked = None;
        egui::Window::new(RichText::new("Keyboard shortcuts").size(base * 1.1))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .frame(Frame::window(&ctx.global_style()).fill(pal.panel_bg).inner_margin(Margin::same(18)))
            .show(ctx, |ui| {
                let open_key = format!("{CMD}+O");
                let find_key = format!("{CMD}+F  /");
                let zoom_key = format!("{CMD} + −  0   {CMD}+scroll");
                let quit_key = format!("q  {CMD}+Q");
                let rows: Vec<(&str, &str)> = vec![
                    (&open_key, "Open file"),
                    (&find_key, "Find in document"),
                    ("Enter  n  N", "Next / previous match"),
                    ("j k  ↓ ↑", "Scroll by line"),
                    ("Space  PgDn / Shift+Space  PgUp", "Scroll by page"),
                    ("d  u", "Scroll half a page"),
                    ("g  G  Home  End", "Top / bottom"),
                    ("o", "Toggle outline sidebar"),
                    ("f", "Toggle files sidebar"),
                    ("t", "Toggle dark / light theme"),
                    ("w   [  ]", "Full-width column / narrower / wider"),
                    (&zoom_key, "Zoom"),
                    ("Alt+←  Backspace  /  Alt+→", "Back / forward (also the arrows in the status bar)"),
                    ("Mouse side buttons", "Back / forward"),
                    ("r  F5", "Reload (files also reload automatically)"),
                    ("Click link", "Open in browser, or follow to another .md"),
                    ("Hover code block", "Copy button"),
                    ("Right-click", "Copy selection, open or copy link"),
                    ("Click width in status bar", "Column width presets (− + step through them)"),
                    ("↑ or folder name in Files", "Show the folder above, or pick one"),
                    (&quit_key, "Quit"),
                ];
                egui::Grid::new("help-grid").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
                    for (k, v) in &rows {
                        ui.label(RichText::new(*k).monospace().color(pal.accent));
                        ui.label(RichText::new(*v).color(pal.text));
                        ui.end_row();
                    }
                });
                ui.add_space(10.0);
                #[cfg(windows)]
                {
                    let mut on = self.setting_on;
                    if ui.checkbox(&mut on, RichText::new("Open files in the window that is already open").color(pal.text)).changed() {
                        clicked = Some(on);
                    }
                    ui.add_space(6.0);
                }
                ui.label(
                    RichText::new(format!("Fonts: {} · {}", self.fonts.sans_name, self.fonts.mono_name))
                        .size(base * 0.8)
                        .color(pal.text_dim),
                );
            });
        #[cfg(windows)]
        if let Some(on) = clicked {
            self.set_single_window(on, ctx);
        }
        self.show_help = open;
    }

    // ------------------------------------------------------------------
    // Reading column
    // ------------------------------------------------------------------

    fn show_content(&mut self, ui: &mut Ui, ctx: &Context) {
        let pal = self.pal;
        let base = self.settings.base_size;

        let Some(doc) = self.doc.clone() else {
            self.content_rect = Rect::NOTHING;
            self.menu_request = None;
            self.show_welcome(ui);
            return;
        };

        // Search bookkeeping
        if self.search.needs_run(self.gen) {
            self.search.run(&doc, self.gen);
            if let Some(b) = self.search.current_block() {
                self.jump_to_block(b);
            }
        }
        if let Some(anchor) = self.pending_anchor.take() {
            if let Some(b) = doc.find_slug(&anchor) {
                self.jump_to_block(b);
            }
        }

        let avail_w = ui.available_width();
        let margin = base * 2.0;
        let col_w = if self.settings.full_width {
            (avail_w - 2.0 * margin).max(200.0)
        } else {
            (avail_w - 2.0 * margin).min(self.settings.column_width).max(200.0)
        };
        self.column_max = (avail_w - 2.0 * margin).max(200.0);
        let left_pad = ((avail_w - col_w) / 2.0).max(margin);
        let key = ((col_w * 2.0).round() as i32, (base * 100.0) as i32);
        if key != self.layout_key {
            self.layout_key = key;
            for m in &mut self.measures {
                m.measured = false;
            }
            self.measure_all = doc.blocks.len() <= 600;
        }

        let mut sa = ScrollArea::vertical()
            .id_salt("content")
            .auto_shrink([false, false])
            .scroll_source(ScrollSource { scroll_bar: true, drag: DragScroll::OnTouch, mouse_wheel: true });
        if let Some(off) = self.pending_offset.take() {
            let measured = self.content_gen == self.gen;
            match scroll_target(off, self.content_h, measured) {
                Some(y) => sa = set_scroll(ui, sa, y, measured),
                None => {
                    self.pending_offset = Some(off);
                    ctx.request_repaint();
                }
            }
        }

        let scroll_delta = std::mem::take(&mut self.scroll_delta);
        let jump = self.jump;
        let measure_all = std::mem::take(&mut self.measure_all);
        let base_dir = self.path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
        let search = if self.search.open && !self.search.matches.is_empty() { Some(&self.search) } else { None };
        let hovered_link = self.hovered_link;

        let mut rc = RenderCtx::new(
            &pal,
            &self.fonts,
            base,
            base_dir.as_deref(),
            self.gen,
            &mut self.hl,
            &mut self.diagrams,
            search,
            hovered_link,
        );
        let measures = &mut self.measures;
        let block_tops = &mut self.block_tops;
        let gap = rc.gap();
        // An image or diagram above a jump's target that is still loading will move it.
        let mut pending_above_target = false;

        let out = sa.show_viewport(ui, |ui, _viewport| {
            if scroll_delta != 0.0 {
                ui.scroll_with_delta(vec2(0.0, -scroll_delta));
            }
            let visible = ui.clip_rect().expand2(vec2(0.0, ui.clip_rect().height().max(400.0)));
            ui.add_space(base * 1.2);
            rc.wide = egui::Rangef::new(ui.max_rect().left() + margin, ui.max_rect().right() - margin);
            let col_rect = Rect::from_min_max(
                pos2(ui.max_rect().left() + left_pad, ui.cursor().top()),
                pos2(ui.max_rect().left() + left_pad + col_w, ui.max_rect().bottom().max(ui.cursor().top() + 100.0)),
            );
            ui.scope_builder(UiBuilder::new().max_rect(col_rect).layout(egui::Layout::top_down(Align::Min)), |ui| {
                ui.set_width(col_w);
                ui.spacing_mut().item_spacing.y = 0.0;
                let n = doc.blocks.len();
                for (i, block) in doc.blocks.iter().enumerate() {
                    let top = ui.cursor().top();
                    block_tops[i] = top;
                    let m = measures[i];
                    let est = if m.measured { m.height } else { estimate_height(block, col_w, base) + gap };
                    let in_view = top <= visible.max.y && top + est >= visible.min.y;
                    let is_target = jump.map(|(b, _)| b == i).unwrap_or(false);
                    if lay_out_block(m, in_view, measure_all, is_target, i + 1 == n) {
                        rc.pending = false;
                        rc.show_block(ui, block);
                        if i + 1 < n {
                            ui.add_space(gap);
                        }
                        let h = ui.cursor().top() - top;
                        measures[i] = Measure { height: h, measured: true, pending: rc.pending };
                        pending_above_target |= rc.pending && jump.is_some_and(|(b, _)| i < b);
                    } else {
                        ui.allocate_space(vec2(col_w, est));
                    }
                    if is_target {
                        let r = Rect::from_min_size(pos2(col_rect.left(), top - base * 0.9), vec2(1.0, 1.0));
                        ui.scroll_to_rect_animation(r, Some(Align::TOP), ScrollAnimation::none());
                    }
                }
            });
            ui.add_space(base * 6.0);
        });

        let actions = std::mem::take(&mut rc.actions);
        let new_hovered = rc.new_hovered_link;
        let hover = std::mem::take(&mut rc.hover);
        drop(rc);

        self.content_rect = out.inner_rect;
        if let Some(pos) = self.menu_request.take() {
            self.menu = Some(ContextMenu { pos, link: hover.link, image: hover.image, code: hover.code, rect: Rect::NOTHING, copy_rect: None });
            // The swallowed press would have taken focus from the search box.
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
            ctx.request_repaint();
        }

        if new_hovered != self.hovered_link {
            self.hovered_link = new_hovered;
            ctx.request_repaint();
        }
        self.scroll_offset = out.state.clamped_offset().y;
        self.viewport_h = out.inner_rect.height();
        self.content_h = out.content_size.y;
        self.content_gen = self.gen;

        if let Some((b, left)) = self.jump {
            if (self.diagrams.laying_out() || pending_above_target) && Instant::now() < self.jump_until {
                // Diagrams grow once laid out, and images once loaded, which moves the target;
                // hold it in place until they are (the diagram worker and egui's image loader
                // repaint when done). Images below the target do not move it, and a slow one
                // there must not keep overriding the user's own scrolling.
            } else if left == 0 {
                self.jump = None;
            } else {
                self.jump = Some((b, left - 1));
                ctx.request_repaint();
            }
        }

        // Current heading for the outline
        let top_y = out.inner_rect.top() + base * 1.5;
        let mut current = None;
        for (hi, h) in doc.headings.iter().enumerate() {
            if h.nested {
                continue;
            }
            if let Some(&t) = self.block_tops.get(h.block) {
                if t <= top_y {
                    current = Some(hi);
                } else {
                    break;
                }
            }
        }
        if current.is_none() && doc.headings.iter().any(|h| !h.nested) {
            current = doc.headings.iter().position(|h| !h.nested);
        }
        if current != self.current_heading {
            self.current_heading = current;
            self.outline_scroll_to = current;
        }

        for a in actions {
            match a {
                Action::OpenLink(url) => self.open_link(&url, ctx),
                Action::Status(s) => self.set_status(s),
            }
        }
    }

    fn handle_screenshot(&mut self, ctx: &Context) {
        let Some(path) = self.screenshot.clone() else { return };
        let shot = self.screenshot_taken.as_ref().and_then(|taken| taken.lock().take());
        if let Some(img) = shot {
            let [w, h] = img.size;
            crate::cli_console(true);
            if self.screenshot_unsettled {
                eprintln!("screenshot taken before the page had finished drawing ({} s past the delay)", SCREENSHOT_WAIT_MAX.as_secs());
            }
            match image::save_buffer(&path, img.as_raw(), w as u32, h as u32, image::ColorType::Rgba8) {
                Ok(()) => eprintln!("screenshot saved to {} ({w}x{h})", path.display()),
                Err(e) => eprintln!("screenshot failed: {e}"),
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
        } else {
            // Diagrams are laid out and drawn, and code highlighted, on worker threads, and
            // images read on egui's loader thread (a block that drew one still loading is
            // re-measured every frame until it arrives, so its flag clears then); a jump to
            // an anchor or a restored scroll position moves the page once the blocks above
            // it are measured. eframe captures the frame painted after the request, and
            // this runs at the end of the frame, after show_content queued this frame's
            // work, so one settled frame is enough. Without a document nothing is loading.
            // A block's pending flag covers its diagrams as well as its images.
            let blocks_loading = self.doc.is_some() && self.measures.iter().any(|m| m.pending);
            let settled = !self.diagrams.busy() && !self.hl.busy() && !blocks_loading && self.jump.is_none() && self.pending_offset.is_none();
            if self.screenshot_taken.is_none() && screenshot_due(self.started.elapsed(), Duration::from_millis(self.screenshot_delay_ms), settled) {
                if !settled {
                    log::warn!("taking the screenshot before the page has finished drawing");
                    self.screenshot_unsettled = true;
                }
                let taken = Arc::new(egui::mutex::Mutex::new(None));
                self.screenshot_taken = Some(taken.clone());
                log::debug!("requesting screenshot at frame {}", self.frames);
                ctx.request_screenshot(move |image| *taken.lock() = Some(image));
            }
            // Keep frames coming so the screenshot is picked up once taken.
            ctx.request_repaint();
        }
    }

    fn show_welcome(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let base = self.settings.base_size;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.label(RichText::new("mdread").size(base * 2.4).color(pal.heading).strong());
            ui.add_space(base * 0.6);
            if let Some(err) = &self.error {
                ui.label(RichText::new(err).color(pal.caution).size(base));
                ui.add_space(base);
            }
            ui.label(RichText::new(format!("Drop a Markdown file here, or press {CMD}+O to open one.")).color(pal.text_dim).size(base * 1.05));
            ui.add_space(base * 0.3);
            ui.label(RichText::new("mdread README.md    ·    cat notes.md | mdread -").monospace().color(pal.text_dim).size(base * 0.9));
            ui.add_space(base * 1.5);
            ui.label(RichText::new("Press ? for keyboard shortcuts").color(pal.text_dim).size(base * 0.9));
        });
    }
}

/// How long `--screenshot` waits past its delay for diagrams, images and highlighting. It
/// must not wait for ever: the 10 s layout limit is per diagram and the worker lays them
/// out one at a time, so a page with several slow ones takes a multiple of it, a worker
/// that died leaves its diagrams pending for good, and a remote image can be slow.
const SCREENSHOT_WAIT_MAX: Duration = Duration::from_secs(20);

/// Whether to take the `--screenshot` now, `elapsed` after launch: once the `delay` has
/// passed and the page is `settled`, or `SCREENSHOT_WAIT_MAX` after the delay regardless.
fn screenshot_due(elapsed: Duration, delay: Duration, settled: bool) -> bool {
    elapsed >= delay && (settled || elapsed >= delay + SCREENSHOT_WAIT_MAX)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frames += 1;
        if self.frames == 1 {
            log::info!("first frame {:.1} ms after launch", self.started.elapsed().as_secs_f64() * 1000.0);
        }
        let frame_t0 = Instant::now();
        self.poll_channels(&ctx);
        self.handle_input(&ctx, frame);
        self.add_ui_script_fallbacks(&ctx);
        #[cfg(windows)]
        self.sync_title_bar(frame);

        self.show_status_bar(ui);
        if self.search.open {
            self.show_search_bar(ui, &ctx);
        }
        if self.settings.show_outline {
            self.show_outline(ui);
        }
        if self.settings.show_files {
            self.show_files_panel(ui, &ctx);
        }
        let pal = self.pal;
        egui::CentralPanel::default()
            .frame(Frame::new().fill(pal.bg))
            .show(ui, |ui| {
                self.show_content(ui, &ctx);
            });
        #[cfg(windows)]
        if let Some((path, arrived)) = self.forward_shown.take() {
            log::info!("forwarded {} shown {:.1} ms after the request arrived", path.display(), arrived.elapsed().as_secs_f64() * 1000.0);
        }
        self.show_context_menu(&ctx);
        self.show_help(&ctx);
        // Last, so that it sees the diagram drawing and highlighting show_content asked for.
        self.handle_screenshot(&ctx);
        log::trace!(
            "frame {} at {:.0} ms took {:.2} ms",
            self.frames,
            self.started.elapsed().as_secs_f64() * 1000.0,
            frame_t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    /// Runs before egui sees the frame's input. Right-presses over the reading column
    /// are swallowed and turned into a context-menu request, because egui collapses
    /// the label text selection on any press over a label. A press on the menu's
    /// "Copy" item becomes an `Event::Copy`, which is how egui copies that selection.
    fn raw_input_hook(&mut self, _ctx: &Context, raw: &mut egui::RawInput) {
        let mut copy = false;
        raw.events.retain(|e| self.keep_event(e, &mut copy));
        if copy {
            raw.events.push(Event::Copy);
            self.set_status("Copied selection to clipboard");
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let mut s = self.settings.clone();
        if self.theme_override.is_some() {
            s.theme = self.saved_theme;
        }
        eframe::set_value(storage, eframe::APP_KEY, &s);
    }

    /// Runs after `save`. Frees the single-window pipe name: from here on
    /// senders get `refused` and claim the role themselves instead of reaching a window
    /// that is going away.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        #[cfg(windows)]
        crate::instance::shutdown();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.pal.bg.to_normalized_gamma_f32()
    }
}

/// What the keyboard (or a mouse button) asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cmd {
    Open,
    Quit,
    Search,
    Reload,
    Back,
    Forward,
    Escape,
    Top,
    Bottom,
    Next,
    Prev,
    Outline,
    Files,
    Theme,
    Width,
    Narrower,
    Wider,
    Help,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// Scroll by this many lines (negative: up).
    Lines(f32),
    /// Scroll by this many pages.
    Pages(f32),
}

/// Whether the held modifiers fit a shortcut. As in egui, an extra Shift is fine, since
/// some layouts need it to type '?', '/' or '+'; so a binding with Shift must be checked
/// before the same key without it. On Windows an extra Alt is not: Alt+letter is a menu
/// chord there, and Ctrl+Alt stands in for AltGr (Ctrl+Alt+Q types '@' on a German
/// keyboard), so neither may reach the plain or Ctrl shortcuts. Real AltGr presses come
/// without Ctrl and Alt: winit clears them.
fn modifiers_fit(held: Modifiers, shortcut: Modifiers) -> bool {
    held.matches_logically(shortcut) && (!cfg!(windows) || held.alt == shortcut.alt)
}

/// `InputState::consume_key`, with [`modifiers_fit`] deciding which presses count.
fn consume(i: &mut egui::InputState, shortcut: Modifiers, key: Key) -> bool {
    let before = i.events.len();
    i.events.retain(|e| {
        !matches!(e, egui::Event::Key { key: k, pressed: true, modifiers, .. } if *k == key && modifiers_fit(*modifiers, shortcut))
    });
    i.events.len() != before
}

/// `InputState::key_pressed` for the navigation keys, which work with any of Ctrl and
/// Shift held, but on Windows not with Alt.
fn pressed(i: &egui::InputState, key: Key) -> bool {
    i.events.iter().any(|e| matches!(e, egui::Event::Key { key: k, pressed: true, modifiers, .. } if *k == key && !(cfg!(windows) && modifiers.alt)))
}

/// Reads the shortcuts pressed this frame. Single-key shortcuts are off while a text
/// field has focus (`typing`).
fn key_commands(i: &mut egui::InputState, typing: bool) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    let cmd = Modifiers::COMMAND;
    let none = Modifiers::NONE;
    let shift = Modifiers::SHIFT;
    if consume(i, cmd, Key::O) {
        cmds.push(Cmd::Open);
    }
    if consume(i, cmd, Key::Q) || consume(i, cmd, Key::W) {
        cmds.push(Cmd::Quit);
    }
    if consume(i, cmd, Key::F) {
        cmds.push(Cmd::Search);
    }
    if consume(i, cmd, Key::R) || consume(i, none, Key::F5) {
        cmds.push(Cmd::Reload);
    }
    if consume(i, Modifiers::ALT, Key::ArrowLeft) {
        cmds.push(Cmd::Back);
    }
    if consume(i, Modifiers::ALT, Key::ArrowRight) {
        cmds.push(Cmd::Forward);
    }
    // Shifted bindings go first everywhere: the plain one would also take Shift+key.
    if consume(i, shift, Key::F3) {
        cmds.push(Cmd::Prev);
    }
    if consume(i, none, Key::F3) {
        cmds.push(Cmd::Next);
    }
    if consume(i, none, Key::F1) {
        cmds.push(Cmd::Help);
    }
    if i.key_pressed(Key::Escape) {
        cmds.push(Cmd::Escape);
    }
    if typing {
        return cmds;
    }
    if consume(i, none, Key::J) || pressed(i, Key::ArrowDown) {
        cmds.push(Cmd::Lines(1.0));
    }
    if consume(i, none, Key::K) || pressed(i, Key::ArrowUp) {
        cmds.push(Cmd::Lines(-1.0));
    }
    if consume(i, shift, Key::Space) || pressed(i, Key::PageUp) {
        cmds.push(Cmd::Pages(-1.0));
    }
    if consume(i, none, Key::Space) || pressed(i, Key::PageDown) {
        cmds.push(Cmd::Pages(1.0));
    }
    if consume(i, none, Key::D) {
        cmds.push(Cmd::Pages(0.5));
    }
    if consume(i, none, Key::U) {
        cmds.push(Cmd::Pages(-0.5));
    }
    if consume(i, shift, Key::G) || pressed(i, Key::End) {
        cmds.push(Cmd::Bottom);
    }
    if consume(i, none, Key::G) || pressed(i, Key::Home) {
        cmds.push(Cmd::Top);
    }
    if consume(i, none, Key::Slash) {
        cmds.push(Cmd::Search);
    }
    if consume(i, shift, Key::N) {
        cmds.push(Cmd::Prev);
    }
    if consume(i, none, Key::N) {
        cmds.push(Cmd::Next);
    }
    if consume(i, none, Key::O) {
        cmds.push(Cmd::Outline);
    }
    if consume(i, none, Key::F) {
        cmds.push(Cmd::Files);
    }
    if consume(i, none, Key::T) {
        cmds.push(Cmd::Theme);
    }
    if consume(i, none, Key::W) {
        cmds.push(Cmd::Width);
    }
    if consume(i, none, Key::OpenBracket) {
        cmds.push(Cmd::Narrower);
    }
    if consume(i, none, Key::CloseBracket) {
        cmds.push(Cmd::Wider);
    }
    if consume(i, none, Key::Q) {
        cmds.push(Cmd::Quit);
    }
    if consume(i, none, Key::R) {
        cmds.push(Cmd::Reload);
    }
    if consume(i, shift, Key::Questionmark) || consume(i, none, Key::Questionmark) {
        cmds.push(Cmd::Help);
    }
    if consume(i, none, Key::Backspace) {
        cmds.push(Cmd::Back);
    }
    if consume(i, none, Key::Plus) || consume(i, none, Key::Equals) || consume(i, shift, Key::Equals) || consume(i, shift, Key::Plus) {
        cmds.push(Cmd::ZoomIn);
    }
    if consume(i, none, Key::Minus) {
        cmds.push(Cmd::ZoomOut);
    }
    if consume(i, none, Key::Num0) {
        cmds.push(Cmd::ZoomReset);
    }
    cmds
}

/// The file a dropped folder opens: its README or index, else the first Markdown file
/// by name. Only this folder is looked at, as the drop and the command line are handled
/// on the UI thread (a forwarded folder is looked at on its worker thread).
pub(crate) fn markdown_in_folder(dir: &Path) -> Option<PathBuf> {
    if let Some(p) = FOLDER_INDEX.iter().map(|n| dir.join(n)).find(|p| p.is_file()) {
        return Some(p);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()) && files::is_markdown(&e.path()))
        .map(|e| e.path())
        .min_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
}

/// Rough height guess for blocks that have not been laid out yet.
fn estimate_height(block: &Block, col_w: f32, base: f32) -> f32 {
    let lh = base * 1.55;
    let chars_per_line = (col_w / (base * 0.5)).max(10.0);
    let para = |chars: usize| ((chars as f32 / chars_per_line).ceil().max(1.0)) * lh;
    match block {
        Block::Heading { level, inline, .. } => {
            let scale = match level {
                1 => 2.0,
                2 => 1.55,
                3 => 1.28,
                _ => 1.1,
            };
            base * 1.4 + para(inline.text.len()) * scale * 0.85
        }
        Block::Paragraph(items) => items
            .iter()
            .map(|it| match it {
                InlineItem::Text(t) => para(t.text.len()),
                InlineItem::Image(_) => lh * 1.2,
            })
            .sum(),
        Block::Code(c) => c.text.lines().count().max(1) as f32 * base * 0.86 * 1.5 + base * 1.3,
        Block::Quote { blocks, .. } => blocks.iter().map(|b| estimate_height(b, col_w - base * 1.3, base) + base * 0.9).sum(),
        Block::List { items, .. } => items
            .iter()
            .map(|it| it.blocks.iter().map(|b| estimate_height(b, col_w - base * 1.6, base) + base * 0.3).sum::<f32>())
            .sum(),
        Block::Table(t) => (t.rows.len() + 1) as f32 * (lh + base * 0.8),
        Block::Rule => base * 1.0,
        Block::Image(_) => base * 12.0,
        Block::Math(_) => lh * 1.5,
        Block::Metadata(_) => lh * 1.2,
        Block::DefList(e) => e.len() as f32 * lh * 2.5,
        Block::Footnotes(f) => f.len() as f32 * lh * 1.5 + base * 2.0,
    }
}

/// Ask egui to repaint after `delay` from a helper thread (timer-based repaints are coarse on some backends).
fn wake_after(ctx: &Context, delay: Duration) {
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("wake".into())
        .spawn(move || {
            std::thread::sleep(delay);
            ctx.request_repaint();
        })
        .ok();
}

/// A `file://` URI as a plain path; anything else unchanged.
fn local_path(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

/// An image URI as an address worth copying: a plain path for a `file://` image.
fn image_address(uri: &str) -> String {
    #[cfg(windows)]
    if let Some(p) = links::file_uri_to_path(uri) {
        return links::display_path(&p);
    }
    local_path(uri).to_owned()
}

/// Whether a watcher event is about the open file. On Windows the name can come in the
/// case the writer used, or as an 8.3 short name (`README~1.MD`), which only counts if
/// it still leads to the open file.
fn is_watched_file(p: &Path, target: &Path) -> bool {
    if p == target {
        return true;
    }
    if !cfg!(windows) || p.parent() != target.parent() {
        return false;
    }
    let lower = |p: &Path| p.as_os_str().to_string_lossy().to_lowercase();
    match p.file_name().map(|n| n.to_string_lossy()) {
        Some(name) if is_short_name(&name) => dunce::canonicalize(p).is_ok_and(|long| lower(&long) == lower(target)),
        Some(_) => lower(p) == lower(target),
        None => false,
    }
}

/// An 8.3 short name: up to eight characters ending in `~` and digits, then an optional
/// extension of up to three (`README~1.MD`). Not `notes.md~`, `~$x.docx` or `.~lock.x#`.
fn is_short_name(name: &str) -> bool {
    let (stem, ext) = name.split_once('.').unwrap_or((name, ""));
    let Some((head, num)) = stem.rsplit_once('~') else { return false };
    !head.is_empty() && stem.len() <= 8 && !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) && ext.len() <= 3 && !ext.contains('.')
}

/// The offset to give the content scroll area for a requested one. `content_h` is the
/// content height from the last frame, and `measured_this_doc` says whether that frame showed
/// the document now loaded.
///
/// The clamp keeps End's `f32::MAX` out of the layout. It only applies to the page it was
/// measured on: on the frame after a document change, `content_h` is the previous
/// document's, and clamping Back's saved offset to it would land a long page near the top.
/// Other offsets go through as they are, and the scroll area clamps them to the new document
/// at the end of the frame. End before the new page has been measured (a reload or the Open
/// dialog can change the document earlier in the same frame) gets `None`: try next frame.
fn scroll_target(requested: f32, content_h: f32, measured_this_doc: bool) -> Option<f32> {
    if measured_this_doc {
        Some(requested.min(content_h.max(0.0)))
    } else if requested == f32::MAX {
        None
    } else {
        Some(requested)
    }
}

/// Whether a block is laid out this frame rather than given its last measured (or estimated)
/// height: it is near the view, every block is being measured, a jump is heading for it, it
/// is the last block and has never been measured, or an image or diagram in it was still
/// loading when it was. Every block is measured on a document's first frame only, when its
/// images are still being read on egui's loader thread; without the pending case, one off
/// screen would keep its 24 px placeholder height, and history, reload, the percentage and
/// the scrollbar would all work from it.
fn lay_out_block(m: Measure, in_view: bool, measure_all: bool, is_target: bool, last: bool) -> bool {
    in_view || measure_all || is_target || m.pending || !m.measured && last
}

/// Puts the content scroll area at `y` this frame (`measured_this_doc` as for `scroll_target`).
///
/// On the first frame of a new document the scroll area still has the previous one's bounds.
/// With rubber-banding on, an offset past them springs back and then coasts on upward, so
/// Back to a page left further down than the previous page is tall lands far too high. Off
/// for this frame, the offset stays put and the scroll area clamps it to the new content.
fn set_scroll(ui: &mut Ui, sa: ScrollArea, y: f32, measured_this_doc: bool) -> ScrollArea {
    if !measured_this_doc {
        ui.spacing_mut().scroll.kinetic.rubber_band = false;
    }
    sa.vertical_scroll_offset(y)
}

/// Read errors a save in progress causes on Windows: the file is locked (sharing or lock
/// violation), delete-pending (access denied) or briefly gone while an editor replaces it.
fn is_transient_read_error(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(2 | 5 | 32 | 33))
}

#[allow(dead_code)]
fn _unused(_: Color32, _: Stroke) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ctrl as egui-winit reports it: Cmd on macOS, Ctrl elsewhere.
    const CTRL: Modifiers = if cfg!(target_os = "macos") {
        Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE }
    } else {
        Modifiers { ctrl: true, command: true, ..Modifiers::NONE }
    };

    #[test]
    fn screenshot_waits_for_the_delay_then_for_the_page_to_settle() {
        let ms = Duration::from_millis;
        let delay = ms(1500);
        // The delay is a minimum, even when there is nothing to wait for.
        assert!(!screenshot_due(ms(1499), delay, true));
        assert!(screenshot_due(delay, delay, true));
        // After it, a page still drawing diagrams, images or highlighting is waited for...
        assert!(!screenshot_due(delay + SCREENSHOT_WAIT_MAX - ms(1), delay, false));
        // ...but not forever.
        assert!(screenshot_due(delay + SCREENSHOT_WAIT_MAX, delay, false));
    }

    #[test]
    fn width_buttons_step_through_presets() {
        use WidthStep::{Full, Preset};
        let wide_window = 2400.0;
        assert_eq!(width_step(860.0, false, wide_window, true), Some(Preset(1200.0)));
        assert_eq!(width_step(1600.0, false, wide_window, true), Some(Full));
        assert_eq!(width_step(860.0, false, wide_window, false), Some(Preset(720.0)));
        assert_eq!(width_step(720.0, false, wide_window, false), None);
        // Widths set with [ and ] step to the nearest preset.
        assert_eq!(width_step(920.0, false, wide_window, true), Some(Preset(1200.0)));
        assert_eq!(width_step(920.0, false, wide_window, false), Some(Preset(860.0)));
        // Full width goes no wider, and narrower goes below the window's width.
        assert_eq!(width_step(wide_window, true, wide_window, true), None);
        assert_eq!(width_step(wide_window, true, wide_window, false), Some(Preset(1600.0)));
        // Presets wider than the window are skipped: the column already fills it.
        assert_eq!(width_step(860.0, false, 1100.0, true), Some(Full));
        assert_eq!(width_step(1100.0, true, 1100.0, false), Some(Preset(860.0)));
    }

    fn keys(presses: &[(Modifiers, Key)], typing: bool) -> Vec<Cmd> {
        let mut i = egui::InputState::default();
        i.events = presses
            .iter()
            .map(|&(modifiers, key)| egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers })
            .collect();
        key_commands(&mut i, typing)
    }

    fn key(modifiers: Modifiers, key: Key) -> Vec<Cmd> {
        keys(&[(modifiers, key)], false)
    }

    #[test]
    fn shifted_shortcuts_are_not_taken_by_the_plain_ones() {
        let shift = Modifiers::SHIFT;
        let none = Modifiers::NONE;
        assert_eq!(key(shift, Key::N), [Cmd::Prev]);
        assert_eq!(key(none, Key::N), [Cmd::Next]);
        assert_eq!(key(shift, Key::G), [Cmd::Bottom]);
        assert_eq!(key(none, Key::G), [Cmd::Top]);
        assert_eq!(key(shift, Key::Space), [Cmd::Pages(-1.0)]);
        assert_eq!(key(none, Key::Space), [Cmd::Pages(1.0)]);
        assert_eq!(key(shift, Key::F3), [Cmd::Prev]);
        assert_eq!(key(none, Key::F3), [Cmd::Next]);
        // Both in one frame: each goes to its own binding.
        assert_eq!(keys(&[(shift, Key::N), (none, Key::N)], false), [Cmd::Prev, Cmd::Next]);
    }

    #[test]
    fn back_to_a_longer_page_restores_its_scroll_position() {
        // Back from a 1,900 px page to one left 9,000 px down: the last frame measured the
        // short page, so its height says nothing about where the long one can scroll.
        assert_eq!(scroll_target(9000.0, 1900.0, false), Some(9000.0));
        // End on the page shown: as far as that page goes.
        assert_eq!(scroll_target(f32::MAX, 1900.0, true), Some(1900.0));
        assert_eq!(scroll_target(500.0, 1900.0, true), Some(500.0));
    }

    /// Runs `frames` headless egui frames of a page `h` tall in a 600 px window, first putting
    /// it at `restore` (offset, measured_this_doc) if given. Returns where it ends up.
    fn scroll_frames(ctx: &Context, time: &mut f64, h: f32, mut restore: Option<(f32, bool)>, frames: usize) -> f32 {
        let mut y = 0.0;
        for _ in 0..frames {
            *time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
                time: Some(*time),
                predicted_dt: 1.0 / 60.0,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let mut sa = ScrollArea::vertical().id_salt("content").auto_shrink([false, false]);
                if let Some((off, measured)) = restore.take() {
                    sa = set_scroll(ui, sa, off, measured);
                }
                y = sa.show(ui, |ui| ui.allocate_space(vec2(100.0, h))).state.clamped_offset().y;
            });
            out.textures_delta.clear();
        }
        y
    }

    #[test]
    fn back_lands_where_it_left_even_from_a_scrollable_page() {
        // On the first frame the scroll area still has the previous page's bounds; with
        // rubber-banding it would spring the restored offset back and then coast up.
        let ctx = Context::default();
        let mut time = 0.0;
        scroll_frames(&ctx, &mut time, 2000.0, None, 3);
        let y = scroll_frames(&ctx, &mut time, 20000.0, Some((9000.0, false)), 90);
        assert!((y - 9000.0).abs() < 1.0, "landed at {y}, not 9000");
    }

    #[test]
    fn end_waits_for_a_new_page_to_be_measured() {
        // End in the frame a reload or the Open dialog changed the document: f32::MAX must
        // not reach the layout, so it waits a frame for the new page's height.
        assert_eq!(scroll_target(f32::MAX, 1900.0, false), None);
    }

    #[test]
    fn blocks_measured_while_loading_are_laid_out_again() {
        let settled = Measure { height: 300.0, measured: true, pending: false };
        let loading = Measure { height: 24.0, measured: true, pending: true };
        // Off screen, after the first frame: an image or diagram that was still loading when
        // the block was measured is laid out until it has its own size...
        assert!(lay_out_block(loading, false, false, false, false));
        // ...while a settled block keeps its height.
        assert!(!lay_out_block(settled, false, false, false, false));
        // The other reasons still hold.
        assert!(lay_out_block(settled, true, false, false, false));
        assert!(lay_out_block(settled, false, true, false, false));
        assert!(lay_out_block(settled, false, false, true, false));
        assert!(lay_out_block(Measure::default(), false, false, false, true));
        assert!(!lay_out_block(settled, false, false, false, true));
        assert!(!lay_out_block(Measure::default(), false, false, false, false));
    }

    #[test]
    fn shift_still_types_symbols() {
        // '?' and '+' need Shift on a US keyboard, '/' on a German one.
        assert_eq!(key(Modifiers::SHIFT, Key::Questionmark), [Cmd::Help]);
        assert_eq!(key(Modifiers::SHIFT, Key::Plus), [Cmd::ZoomIn]);
        assert_eq!(key(Modifiers::SHIFT, Key::Slash), [Cmd::Search]);
        assert_eq!(key(Modifiers::NONE, Key::OpenBracket), [Cmd::Narrower]);
    }

    #[test]
    fn ctrl_shortcuts_and_typing() {
        assert_eq!(key(CTRL, Key::O), [Cmd::Open]);
        assert_eq!(key(CTRL, Key::Q), [Cmd::Quit]);
        assert_eq!(key(Modifiers::ALT, Key::ArrowLeft), [Cmd::Back]);
        // Ctrl+N is not a plain N.
        assert_eq!(key(CTRL, Key::N), []);
        // A focused text field keeps the single keys but not the Ctrl ones.
        assert_eq!(keys(&[(Modifiers::NONE, Key::N), (Modifiers::NONE, Key::Q)], true), []);
        assert_eq!(keys(&[(CTRL, Key::F)], true), [Cmd::Search]);
    }

    #[cfg(windows)]
    #[test]
    fn alt_chords_trigger_nothing_on_windows() {
        let alt = Modifiers::ALT;
        let ctrl_alt = CTRL | Modifiers::ALT;
        for k in [Key::F, Key::O, Key::Q, Key::Space, Key::N, Key::Num0, Key::Backspace, Key::ArrowDown, Key::Home] {
            assert_eq!(key(alt, k), [], "Alt+{k:?}");
        }
        // Ctrl+Alt is AltGr: Ctrl+Alt+Q types '@' on a German keyboard.
        for k in [Key::Q, Key::W, Key::O, Key::F, Key::R] {
            assert_eq!(key(ctrl_alt, k), [], "Ctrl+Alt+{k:?}");
        }
        assert_eq!(key(Modifiers::ALT, Key::ArrowRight), [Cmd::Forward]);
        // Ctrl with the navigation keys still scrolls.
        assert_eq!(key(CTRL, Key::End), [Cmd::Bottom]);
    }

    #[test]
    fn dropped_folder_opens_its_readme_or_first_markdown_file() {
        let dir = std::env::temp_dir().join(format!("mdread-drop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(markdown_in_folder(&dir), None);
        std::fs::write(dir.join("sub").join("a.md"), "").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        assert_eq!(markdown_in_folder(&dir), None);
        std::fs::write(dir.join("Zeta.md"), "").unwrap();
        std::fs::write(dir.join("beta.markdown"), "").unwrap();
        assert_eq!(markdown_in_folder(&dir), Some(dir.join("beta.markdown")));
        std::fs::write(dir.join("index.md"), "").unwrap();
        assert_eq!(markdown_in_folder(&dir), Some(dir.join("index.md")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_names_are_told_from_editor_temp_files() {
        for name in ["README~1.MD", "NOTES~12.M", "AB~1", "PROGRA~1"] {
            assert!(is_short_name(name), "{name}");
        }
        for name in ["notes.md~", "~$report.docx", ".~lock.x#", "README~1.MARKDOWN", "LONGNAME~1.MD", "a~b.md", "x~1.tar.gz"] {
            assert!(!is_short_name(name), "{name}");
        }
    }

    #[test]
    fn a_failed_first_load_drops_its_anchor() {
        // `mdread folder.md#x`: the path exists, so `#x` is split off, but it cannot be read.
        // The anchor belonged to that file; the next document opened must not jump to x.
        let dir = std::env::temp_dir().join(format!("mdread-anchor-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("folder.md")).unwrap();
        let next = dir.join("next.md");
        std::fs::write(&next, "# Top\n\n## x\n").unwrap();
        let ctx = Context::default();
        let args = Args { files: vec![dir.join("folder.md")], anchor: Some("x".into()), ..Args::default() };
        let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), args);
        assert!(app.error.is_some(), "the folder should fail to load");
        app.load_path(next, true, &ctx);
        assert!(app.doc.is_some());
        assert_eq!(app.pending_anchor, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An app showing `a.md` from a fresh temp folder, and a path in it that cannot be read.
    fn app_showing_a_page(tag: &str) -> (App, Context, PathBuf, PathBuf, PathBuf) {
        let dir = dunce::canonicalize(std::env::temp_dir()).unwrap().join(format!("mdread-{tag}-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.md");
        std::fs::write(&a, "# A\n").unwrap();
        let ctx = Context::default();
        let args = Args { files: vec![a.clone()], ..Args::default() };
        let app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), args);
        assert!(app.doc.is_some(), "a.md should load");
        let missing = dir.join("missing.md");
        (app, ctx, dir, a, missing)
    }

    #[test]
    fn failed_open_keeps_the_shown_page() {
        let (mut app, ctx, dir, a, missing) = app_showing_a_page("keep-page");
        app.pending_anchor = Some("old".into());
        assert!(!app.load_path(missing, true, &ctx));
        assert!(app.doc.is_some());
        assert!(app.error.is_none());
        assert_eq!(app.path, Some(a));
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history_pos, 0);
        assert!(app.status.as_ref().is_some_and(|(s, _)| s.starts_with("Cannot read ")), "{:?}", app.status);
        assert_eq!(app.pending_anchor, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_open_from_the_welcome_screen_shows_the_error_screen() {
        let dir = dunce::canonicalize(std::env::temp_dir()).unwrap().join(format!("mdread-welcome-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = Context::default();
        let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), Args::default());
        assert!(app.doc.is_none());
        assert!(!app.load_path(dir.join("missing.md"), true, &ctx));
        assert!(app.error.is_some());
        assert!(app.doc.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_history_move_still_shows_the_error_screen() {
        let (mut app, ctx, dir, _a, missing) = app_showing_a_page("history-move");
        assert!(!app.load_path(missing, false, &ctx));
        assert!(app.error.is_some());
        assert!(app.doc.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn clicking_a_link_to_an_unreadable_file_keeps_the_page_and_drops_the_anchor() {
        use std::os::windows::fs::OpenOptionsExt;
        // The target must exist, or `open_local` stops at "Not found" before it reads
        // anything. Holding it open with no sharing makes the read itself fail.
        let (mut app, ctx, dir, a, _missing) = app_showing_a_page("link-load");
        let locked = dir.join("locked.md");
        std::fs::write(&locked, "# Locked\n").unwrap();
        let _hold = std::fs::OpenOptions::new().read(true).share_mode(0).open(&locked).unwrap();
        assert!(locked.is_file());
        assert!(std::fs::read(&locked).is_err(), "the held file should not be readable");
        app.pending_anchor = Some("old".into());
        let shown = app.doc.clone().unwrap();
        app.open_local(locked, Some("x".into()), &ctx);
        // The same parsed page is still shown, so the title (set only with a new page or
        // the error screen) is unchanged too.
        assert!(app.doc.as_ref().is_some_and(|d| Arc::ptr_eq(d, &shown)));
        assert!(app.error.is_none());
        assert_eq!(app.path, Some(a));
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history_pos, 0);
        assert!(app.status.as_ref().is_some_and(|(s, _)| s.starts_with("Cannot read ")), "{:?}", app.status);
        assert_eq!(app.pending_anchor, None);
        drop(_hold);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh temp folder for the command-line tests.
    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = dunce::canonicalize(std::env::temp_dir()).unwrap().join(format!("mdread-{tag}-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn app_with_args(args: Args) -> App {
        App::new(&eframe::CreationContext::_new_kittest(Context::default()), args)
    }

    #[test]
    fn command_line_folder_opens_its_readme() {
        let dir = fresh_dir("cli-folder");
        std::fs::write(dir.join("README.md"), "# Hello\n").unwrap();
        let app = app_with_args(Args { files: vec![dir.clone()], ..Args::default() });
        assert!(app.doc.is_some());
        assert!(app.path.as_ref().is_some_and(|p| p.ends_with("README.md")), "{:?}", app.path);
        assert!(app.error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_line_empty_folder_sets_a_status() {
        let dir = fresh_dir("cli-empty");
        let app = app_with_args(Args { files: vec![dir.clone()], ..Args::default() });
        assert!(app.doc.is_none());
        assert!(app.status.as_ref().is_some_and(|(s, _)| s.starts_with("No Markdown files in")), "{:?}", app.status);
        assert!(app.error.as_ref().is_some_and(|e| e.starts_with("No Markdown files in")), "{:?}", app.error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_without_a_file_still_opens_search() {
        let app = app_with_args(Args { find: Some("x".into()), ..Args::default() });
        assert!(app.doc.is_none());
        assert!(app.search.open);
        assert_eq!(app.search.query, "x");
    }

    #[test]
    fn find_is_not_applied_after_a_failed_load() {
        let dir = fresh_dir("find-failed");
        let app = app_with_args(Args { files: vec![dir.join("missing.md")], find: Some("x".into()), ..Args::default() });
        assert!(app.doc.is_none());
        assert!(!app.search.open);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_is_applied_after_a_successful_load() {
        let dir = fresh_dir("find-ok");
        let a = dir.join("a.md");
        std::fs::write(&a, "# A\n").unwrap();
        let app = app_with_args(Args { files: vec![a], find: Some("x".into()), ..Args::default() });
        assert!(app.doc.is_some());
        assert!(app.search.open);
        assert_eq!(app.search.query, "x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn anchor_follows_the_folder_readme() {
        let dir = fresh_dir("cli-folder-anchor");
        std::fs::write(dir.join("README.md"), "# Top\n\n## x\n").unwrap();
        let app = app_with_args(Args { files: vec![dir.clone()], anchor: Some("x".into()), ..Args::default() });
        assert!(app.doc.is_some());
        assert_eq!(app.pending_anchor.as_deref(), Some("x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn command_line_lnk_opens_its_target() {
        let _com = crate::win::ComApartment::init();
        let dir = fresh_dir("cli-lnk");
        let target = dir.join("notes.md");
        std::fs::write(&target, "# Notes\n").unwrap();
        let lnk = dir.join("notes.md.lnk");
        assert!(crate::win::create_shortcut(&lnk, &target));
        let app = app_with_args(Args { files: vec![lnk], ..Args::default() });
        assert!(app.doc.is_some());
        assert_eq!(app.path, Some(target));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn command_line_bogus_lnk_sets_a_status() {
        let dir = fresh_dir("cli-bad-lnk");
        let lnk = dir.join("bogus.lnk");
        std::fs::write(&lnk, "not a shortcut").unwrap();
        let app = app_with_args(Args { files: vec![lnk], ..Args::default() });
        assert!(app.doc.is_none());
        assert!(app.status.as_ref().is_some_and(|(s, _)| s.ends_with("does not point to a file")), "{:?}", app.status);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the pipe thread and the worker make of a forwarded open, done here on the test
    /// thread: a ticket taken at "commit", then `resolve_forward`.
    #[cfg(windows)]
    fn forwarded(app: &App, path: &Path, anchor: Option<&str>, find: Option<&str>) -> crate::instance::ForwardResult {
        let request = crate::instance::Request { path: path.to_path_buf(), anchor: anchor.map(Into::into), find: find.map(Into::into) };
        let gen = take_ticket(&app.open_tickets);
        let outcome = crate::instance::resolve_forward(&request);
        crate::instance::ForwardResult { gen, request, outcome, arrived: Instant::now() }
    }

    #[cfg(windows)]
    #[test]
    fn forward_opens_a_new_file_and_back_returns() {
        let (mut app, ctx, dir, a, _missing) = app_showing_a_page("fwd-new");
        let b = dir.join("b.md");
        std::fs::write(&b, "# B\n").unwrap();
        let nav_before = app.nav_gen;
        let r = forwarded(&app, &b, None, None);
        app.apply_forward(r, &ctx);
        assert_eq!(app.path.as_ref(), Some(&b));
        assert_eq!(app.history.len(), 2);
        assert_eq!(app.nav_gen, nav_before, "applying a forward is not a UI navigation");
        app.go_history(-1, &ctx);
        assert!(app.nav_gen > nav_before);
        assert_eq!(app.path, Some(a));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn forward_of_the_shown_file_does_not_reload_or_duplicate() {
        let (mut app, ctx, dir, a, _missing) = app_showing_a_page("fwd-same");
        std::fs::write(&a, "# A\n\n## x\n").unwrap();
        app.reload(&ctx, false);
        let gen = app.gen;
        let r = forwarded(&app, &a, None, None);
        app.apply_forward(r, &ctx);
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.gen, gen, "the document must not be parsed again");
        assert!(app.jump.is_none());
        let r = forwarded(&app, &a, Some("x"), None);
        app.apply_forward(r, &ctx);
        assert_eq!(app.history.len(), 1);
        assert!(app.jump.is_some(), "the anchor jumps in the shown page");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn forward_unreadable_keeps_the_page_with_a_status() {
        let (mut app, ctx, dir, a, missing) = app_showing_a_page("fwd-missing");
        let r = forwarded(&app, &missing, Some("x"), Some("y"));
        app.apply_forward(r, &ctx);
        assert!(app.doc.is_some());
        assert!(app.error.is_none());
        assert_eq!(app.path, Some(a));
        assert_eq!(app.history.len(), 1);
        assert!(app.status.as_ref().is_some_and(|(s, _)| s.starts_with("Cannot read ")), "{:?}", app.status);
        assert!(!app.search.open, "find is not applied after a failed load");
        assert_eq!(app.pending_anchor, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn forward_from_the_welcome_screen_shows_the_error_screen() {
        let dir = fresh_dir("fwd-welcome");
        let ctx = Context::default();
        let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), Args::default());
        let r = forwarded(&app, &dir.join("missing.md"), None, None);
        app.apply_forward(r, &ctx);
        assert!(app.doc.is_none());
        assert!(app.path.is_none());
        assert!(app.error.as_ref().is_some_and(|e| e.starts_with("Cannot read ")), "{:?}", app.error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_slow_forward_result_is_dropped_after_a_newer_open() {
        let (mut app, ctx, dir, _a, _missing) = app_showing_a_page("fwd-slow");
        let (slow, other) = (dir.join("slow.md"), dir.join("other.md"));
        std::fs::write(&slow, "# Slow\n").unwrap();
        std::fs::write(&other, "# Other\n").unwrap();
        let r = forwarded(&app, &slow, None, None);
        assert!(app.load_path(other.clone(), true, &ctx));
        app.apply_forward(r, &ctx);
        assert_eq!(app.path, Some(other));
        assert_eq!(app.history.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_forward_result_is_dropped_after_go_history() {
        let (mut app, ctx, dir, a, _missing) = app_showing_a_page("fwd-history");
        let (slow, b) = (dir.join("slow.md"), dir.join("b.md"));
        std::fs::write(&slow, "# Slow\n").unwrap();
        std::fs::write(&b, "# B\n").unwrap();
        assert!(app.load_path(b, true, &ctx));
        // Committed after the open of b, so only `go_history`'s own navigation makes it stale.
        let r = forwarded(&app, &slow, None, None);
        app.go_history(-1, &ctx);
        app.apply_forward(r, &ctx);
        assert_eq!(app.path, Some(a));
        assert_eq!(app.history.len(), 2);
        assert_eq!(app.history_pos, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_forward_does_not_make_an_older_one_stale() {
        let (mut app, ctx, dir, _a, missing) = app_showing_a_page("fwd-err-order");
        let b = dir.join("b.md");
        std::fs::write(&b, "# B\n").unwrap();
        let rb = forwarded(&app, &b, None, None);
        let rmissing = forwarded(&app, &missing, None, None);
        app.apply_forward(rmissing, &ctx);
        app.apply_forward(rb, &ctx);
        assert_eq!(app.path.as_ref(), Some(&b));
        assert_eq!(app.history.len(), 2);
        assert_eq!(app.history[1].path, b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_forward_retries_the_file_of_the_error_screen() {
        // Started on a file that did not exist yet; the user creates it and opens it again.
        let dir = fresh_dir("fwd-retry");
        let missing = dir.join("later.md");
        let ctx = Context::default();
        let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), Args { files: vec![missing.clone()], ..Args::default() });
        assert!(app.doc.is_none());
        assert!(app.error.is_some());
        assert_eq!(app.path.as_ref(), Some(&missing));
        std::fs::write(&missing, "# Later\n").unwrap();
        let r = forwarded(&app, &missing, None, None);
        app.apply_forward(r, &ctx);
        assert!(app.doc.is_some());
        assert!(app.error.is_none());
        assert_eq!(app.history.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn accept_refuses_a_hung_window_and_a_full_worker_cap() {
        let count = AtomicUsize::new(0);
        assert!(accepts_forward(&count, false));
        assert!(!accepts_forward(&count, true), "a hung window refuses");
        count.store(MAX_FORWARD_WORKERS - 1, Ordering::SeqCst);
        assert!(accepts_forward(&count, false), "one place is still free");
        count.store(MAX_FORWARD_WORKERS, Ordering::SeqCst);
        assert!(!accepts_forward(&count, false), "every place is taken: the sender keeps its file");
    }

    #[cfg(windows)]
    #[test]
    fn worker_slots_are_capped_and_given_back() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut held: Vec<WorkerSlot> = (0..MAX_FORWARD_WORKERS).map(|_| WorkerSlot::take(&count, MAX_FORWARD_WORKERS).expect("a free slot")).collect();
        assert!(WorkerSlot::take(&count, MAX_FORWARD_WORKERS).is_none(), "all slots are taken");
        held.pop();
        assert_eq!(count.load(Ordering::SeqCst), MAX_FORWARD_WORKERS - 1);
        let again = WorkerSlot::take(&count, MAX_FORWARD_WORKERS);
        assert!(again.is_some());
        drop(again);
        drop(held);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[cfg(windows)]
    #[test]
    fn a_current_forward_result_is_applied() {
        let (mut app, ctx, dir, _a, _missing) = app_showing_a_page("fwd-current");
        let b = dir.join("b.md");
        std::fs::write(&b, "# B\n").unwrap();
        let r = forwarded(&app, &b, None, None);
        app.apply_forward(r, &ctx);
        assert_eq!(app.path, Some(b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn forwards_committed_together_all_land_in_history() {
        // Explorer multi-select: each request is committed before the earlier results are
        // applied; every file is pushed, in the order the results arrive, and the last is shown.
        let (mut app, ctx, dir, a, _missing) = app_showing_a_page("fwd-multi");
        let (b, c) = (dir.join("b.md"), dir.join("c.md"));
        std::fs::write(&b, "# B\n").unwrap();
        std::fs::write(&c, "# C\n").unwrap();
        // The old page was scrolled; no frame draws in between the two results.
        app.scroll_offset = 500.0;
        let rb = forwarded(&app, &b, None, None);
        let rc = forwarded(&app, &c, None, None);
        app.apply_forward(rb, &ctx);
        app.apply_forward(rc, &ctx);
        assert_eq!(app.path.as_ref(), Some(&c));
        let paths: Vec<&Path> = app.history.iter().map(|v| v.path.as_path()).collect();
        assert_eq!(paths, [a.as_path(), &b, &c]);
        assert_eq!(app.history[0].offset, 500.0);
        assert_eq!(app.history[1].offset, 0.0, "b was never scrolled");
        // Results that finish out of order apply in the order they reach the UI thread: both
        // land, the last one to arrive shown.
        let (d, e) = (dir.join("d.md"), dir.join("e.md"));
        std::fs::write(&d, "# D\n").unwrap();
        std::fs::write(&e, "# E\n").unwrap();
        let rd = forwarded(&app, &d, None, None);
        let re = forwarded(&app, &e, None, None);
        app.apply_forward(re, &ctx);
        app.apply_forward(rd, &ctx);
        assert_eq!(app.path.as_ref(), Some(&d));
        let paths: Vec<&Path> = app.history.iter().map(|v| v.path.as_path()).collect();
        assert_eq!(paths, [a.as_path(), &b, &c, &e, &d]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn find_rules() {
        let (mut app, ctx, dir, a, missing) = app_showing_a_page("fwd-find");
        // Same file: the search opens with the term, replacing the query, focused.
        app.search.query = "old".into();
        let r = forwarded(&app, &a, None, Some("term"));
        app.apply_forward(r, &ctx);
        assert!(app.search.open);
        assert_eq!(app.search.query, "term");
        assert!(app.search.focus_pending);
        // An empty find leaves an open search alone.
        app.search.focus_pending = false;
        let r = forwarded(&app, &a, None, None);
        app.apply_forward(r, &ctx);
        assert!(app.search.open);
        assert_eq!(app.search.query, "term");
        let r = forwarded(&app, &a, None, Some(""));
        app.apply_forward(r, &ctx);
        assert_eq!(app.search.query, "term");
        assert!(!app.search.focus_pending);
        // A failed load does not apply find.
        let r = forwarded(&app, &missing, None, Some("other"));
        app.apply_forward(r, &ctx);
        assert_eq!(app.search.query, "term");
        // A successful load does.
        let b = dir.join("b.md");
        std::fs::write(&b, "# B\n").unwrap();
        let r = forwarded(&app, &b, None, Some("bee"));
        app.apply_forward(r, &ctx);
        assert_eq!(app.path, Some(b));
        assert_eq!(app.search.query, "bee");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn forward_folder_and_lnk() {
        let _com = crate::win::ComApartment::init();
        let dir = fresh_dir("fwd-folder");
        let notes = dir.join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("README.md"), "# Notes\n\n## x\n").unwrap();
        let req = |p: &Path| crate::instance::Request { path: p.to_path_buf(), anchor: None, find: None };
        let opened = crate::instance::resolve_forward(&req(&notes)).unwrap_or_else(|e| panic!("the folder's README: {e}"));
        assert_eq!(opened.path, notes.join("README.md"));
        assert!(opened.decoded.text.starts_with("# Notes"));
        assert!(opened.stamp.is_some());
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let err = crate::instance::resolve_forward(&req(&empty)).err().expect("an empty folder fails");
        assert!(err.starts_with("No Markdown files in "), "{err}");
        let lnk = dir.join("notes.lnk");
        assert!(crate::win::create_shortcut(&lnk, &notes.join("README.md")));
        let opened = crate::instance::resolve_forward(&req(&lnk)).unwrap_or_else(|e| panic!("the shortcut's target: {e}"));
        assert_eq!(opened.path, notes.join("README.md"));
        let bogus = dir.join("bogus.lnk");
        std::fs::write(&bogus, "not a shortcut").unwrap();
        let err = crate::instance::resolve_forward(&req(&bogus)).err().expect("a bogus shortcut fails");
        assert!(err.ends_with("does not point to a file"), "{err}");
        // Through the app: the folder's README is shown, with the anchor waiting for it.
        let ctx = Context::default();
        let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), Args::default());
        let r = forwarded(&app, &notes, Some("x"), None);
        app.apply_forward(r, &ctx);
        assert_eq!(app.path, Some(notes.join("README.md")));
        assert_eq!(app.pending_anchor.as_deref(), Some("x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn refused_paths_are_not_opened() {
        let dir = fresh_dir("fwd-refused");
        let con = crate::instance::make_absolute(Path::new("CON"));
        for p in [PathBuf::from(r"\\.\pipe\x"), PathBuf::from(r"\\localhost\pipe\x"), PathBuf::from(r"\\?\UNC\localhost\pipe\x"), con, dir.join("CON"), dir.join("nul.md")] {
            let req = crate::instance::Request { path: p.clone(), anchor: None, find: None };
            let err = crate::instance::resolve_forward(&req).err().unwrap_or_else(|| panic!("{} must be refused", p.display()));
            assert!(err.starts_with("Unsupported path: "), "{}: {err}", p.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn started_same_window_comes_from_args() {
        assert!(app_with_args(Args { window: Some(WindowChoice::SameWindow), ..Args::default() }).started_same_window);
        assert!(!app_with_args(Args { window: Some(WindowChoice::NewWindow), ..Args::default() }).started_same_window);
        let app = app_with_args(Args::default());
        assert!(!app.started_same_window);
        // Headless: no window handle, so nothing is served.
        assert!(app.hwnd.is_none() && app.server.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn forward_closes_the_context_menu() {
        let (mut app, ctx, dir, _a, _missing) = app_showing_a_page("fwd-menu");
        let b = dir.join("b.md");
        std::fs::write(&b, "# B\n").unwrap();
        app.menu = Some(ContextMenu { pos: Pos2::ZERO, link: None, image: None, code: None, rect: Rect::NOTHING, copy_rect: None });
        let r = forwarded(&app, &b, None, None);
        app.apply_forward(r, &ctx);
        assert!(app.menu.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn only_the_open_file_counts_as_watched() {
        let dir = dunce::canonicalize(std::env::temp_dir()).unwrap().join(format!("mdread-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("notes.md");
        for name in ["notes.md", "other.md", "notes.md~", "~$notes.md"] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        assert!(is_watched_file(&target, &target));
        assert!(is_watched_file(&dir.join("NOTES.MD"), &target));
        for name in ["other.md", "notes.md~", "~$notes.md", "NOTES~9.MD"] {
            assert!(!is_watched_file(&dir.join(name), &target), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A headless window wired to a test pipe name and a test registry key, so the setting
    /// tests never touch the real `SingleWindow` value or a real mdread's pipe. On drop it
    /// stops whatever this window serves and waits for the name to be free, also when the
    /// test fails, then deletes the key.
    #[cfg(windows)]
    struct TestWindow {
        app: App,
        ctx: Context,
        name: String,
        key: crate::instance::tests::TestKey,
        _serial: std::sync::RwLockReadGuard<'static, ()>,
    }

    #[cfg(windows)]
    impl TestWindow {
        fn new(tag: &str, args: Args) -> TestWindow {
            let serial = crate::instance::pipe_tests::shared();
            let ctx = Context::default();
            let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), args);
            let key = crate::instance::tests::TestKey::new(&format!("app-{tag}"));
            let name = crate::instance::pipe_tests::test_name();
            app.pipe_name = Some(name.clone());
            app.setting_subkey = Some(key.0.clone());
            TestWindow { app, ctx, name, key, _serial: serial }
        }

        fn set(&mut self, on: bool) {
            self.app.set_single_window(on, &self.ctx);
        }

        fn saved(&self) -> bool {
            crate::instance::read_setting_at(&self.key.0)
        }

        /// Whether some pipe holds the name: creating it again fails with `Taken`.
        fn name_taken(&self) -> bool {
            let id = crate::instance::own_identity().expect("own_identity");
            match crate::instance::create_pipe(&self.name, &id) {
                Ok(_pipe) => false,
                Err(crate::instance::CreateError::Taken) => true,
                Err(crate::instance::CreateError::Other(e)) => panic!("create_pipe: {e}"),
            }
        }

        /// Waits up to 2 s for a server this window stopped to free the name. It normally
        /// does within `stop_serving`'s own 200 ms wait, but a busy test machine may be slower.
        fn name_freed(&self) -> bool {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                let finished = self.app.stopped_server.as_ref().is_none_or(|s| s.is_finished());
                if finished && !self.name_taken() {
                    return true;
                }
                if Instant::now() >= until {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn send(&self, path: &Path) -> crate::instance::ForwardOutcome {
            let id = crate::instance::own_identity().expect("own_identity");
            let req = crate::instance::Request { path: path.to_path_buf(), anchor: None, find: None };
            crate::instance::forward(&self.name, &id, &req, Instant::now() + Duration::from_secs(5))
        }

        /// Forwards `path` and checks that this window took it: `Delivered`, and the worker's
        /// result on the window's channel.
        fn delivers(&self, path: &Path) {
            assert_eq!(self.send(path), crate::instance::ForwardOutcome::Delivered);
            let r = self.app.forward_rx.recv_timeout(Duration::from_secs(5)).expect("the forward reaches this window");
            assert_eq!(r.request.path, path);
        }

        fn status(&self) -> Option<&str> {
            self.app.status.as_ref().map(|(s, _)| s.as_str())
        }
    }

    #[cfg(windows)]
    impl Drop for TestWindow {
        fn drop(&mut self) {
            for server in [self.app.server.take(), self.app.stopped_server.take()].into_iter().flatten() {
                server.stop_serving();
                if !crate::instance::pipe_tests::wait_finished(&server) && !std::thread::panicking() {
                    panic!("the window's server did not finish");
                }
            }
        }
    }

    /// A file for forwards to open.
    #[cfg(windows)]
    fn page(tag: &str) -> (PathBuf, PathBuf) {
        let dir = fresh_dir(tag);
        let file = dir.join("page.md");
        std::fs::write(&file, "# Page\n").unwrap();
        (dir, file)
    }

    #[cfg(windows)]
    #[test]
    fn turning_on_starts_serving_when_nobody_does() {
        let (dir, file) = page("set-on");
        let mut w = TestWindow::new("on", Args::default());
        assert!(!w.name_taken());
        w.set(true);
        assert!(w.saved() && w.app.setting_on);
        assert!(w.app.server.is_some());
        assert!(w.name_taken());
        w.delivers(&file);
        assert_eq!(w.status(), None);
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn turning_on_does_nothing_when_another_window_receives() {
        let mut w = TestWindow::new("on-taken", Args::default());
        let other = crate::instance::pipe_tests::Serving::start(&w.name, true);
        w.set(true);
        assert!(w.saved() && w.app.setting_on);
        assert!(w.app.server.is_none());
        assert_eq!(w.status(), None, "nothing to say: the other window receives");
        assert_eq!(w.send(Path::new(r"C:\x.md")), crate::instance::ForwardOutcome::Delivered);
        assert!(other.commits.recv_timeout(Duration::from_secs(5)).is_ok(), "the other window got it");
    }

    #[cfg(windows)]
    #[test]
    fn turning_off_stops_a_receiver_started_by_the_setting() {
        let mut w = TestWindow::new("off", Args::default());
        w.set(true);
        assert!(w.name_taken());
        w.set(false);
        assert!(!w.saved() && !w.app.setting_on);
        assert!(w.app.server.is_none());
        assert!(w.name_freed(), "the name is free again");
        assert_eq!(w.send(Path::new(r"C:\x.md")), crate::instance::ForwardOutcome::NoReceiver);
    }

    #[cfg(windows)]
    #[test]
    fn turning_off_keeps_a_same_window_receiver() {
        let (dir, file) = page("set-off-same");
        let mut w = TestWindow::new("off-same", Args { window: Some(WindowChoice::SameWindow), ..Args::default() });
        assert!(w.app.started_same_window);
        // As `main` and `App::new` do for a `--same-window` launch that claimed.
        let id = crate::instance::own_identity().unwrap();
        let pipe = crate::instance::create_pipe(&w.name, &id).unwrap();
        w.app.start_serving(pipe, &w.ctx);
        w.set(false);
        assert!(!w.saved() && !w.app.setting_on);
        assert!(w.app.server.is_some(), "a --same-window window keeps listening");
        assert!(w.name_taken());
        w.delivers(&file);
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `--same-window` window that claimed at launch: switching the setting on and off
    /// again, twice, never stops it receiving.
    #[cfg(windows)]
    #[test]
    fn on_off_on_off_in_a_same_window_window_keeps_listening() {
        let (dir, file) = page("set-same-twice");
        let mut w = TestWindow::new("same-twice", Args { window: Some(WindowChoice::SameWindow), ..Args::default() });
        // As `main` and `App::new` do for a `--same-window` launch that claimed.
        let id = crate::instance::own_identity().unwrap();
        let pipe = crate::instance::create_pipe(&w.name, &id).unwrap();
        w.app.start_serving(pipe, &w.ctx);
        assert!(w.name_taken());
        w.delivers(&file);
        for _ in 0..2 {
            w.set(true);
            assert!(w.saved() && w.name_taken());
            w.delivers(&file);
            w.set(false);
            assert!(!w.saved());
            assert!(w.app.server.is_some() && w.name_taken(), "still listening after switching it off");
            w.delivers(&file);
        }
        assert_eq!(w.status(), None);
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn turning_on_in_a_stdin_window_does_not_serve() {
        // A real `-` launch would read this process's stdin in `App::new`; the flag is what
        // `App::new` sets from `args.stdin`. `--screenshot` windows are the same.
        assert!(app_with_args(Args { screenshot: Some(PathBuf::from("x.png")), ..Args::default() }).never_receives);
        assert!(!app_with_args(Args::default()).never_receives);
        let mut w = TestWindow::new("on-stdin", Args::default());
        w.app.never_receives = true;
        w.set(true);
        assert!(w.saved() && w.app.setting_on, "the value is still written");
        assert!(w.app.server.is_none());
        assert!(!w.name_taken());
    }

    /// A receiver the setting started: off frees the name, on claims it again, twice.
    #[cfg(windows)]
    #[test]
    fn turning_off_then_on_restarts_serving() {
        let (dir, file) = page("set-off-on");
        let mut w = TestWindow::new("off-on", Args::default());
        for _ in 0..2 {
            w.set(true);
            assert!(w.name_taken());
            assert!(w.app.server.is_some());
            w.delivers(&file);
            w.set(false);
            assert!(w.name_freed());
        }
        assert_eq!(w.status(), None);
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Re-enabling: a server whose thread is still busy after `stop_serving` keeps
    /// the name, so turning the setting on again does not claim and says when it will.
    #[cfg(windows)]
    #[test]
    fn turning_off_then_on_while_the_old_server_runs_says_so() {
        let mut w = TestWindow::new("off-on-busy", Args::default());
        // A server stuck in `commit` (as if a file took long to hand over) until `release`.
        let (entered_tx, entered) = mpsc::channel::<()>();
        let (release, release_rx) = mpsc::channel::<()>();
        let (entered_tx, release_rx) = (std::sync::Mutex::new(entered_tx), std::sync::Mutex::new(release_rx));
        let id = crate::instance::own_identity().unwrap();
        let pipe = crate::instance::create_pipe(&w.name, &id).unwrap();
        w.app.server = Some(pipe.serve(
            Arc::new(|_: &crate::instance::Request| true),
            Arc::new(move |_| {
                let _ = entered_tx.lock().unwrap_or_else(|e| e.into_inner()).send(());
                let _ = release_rx.lock().unwrap_or_else(|e| e.into_inner()).recv_timeout(Duration::from_secs(5));
            }),
        ));
        w.app.setting_on = true;
        let name = w.name.clone();
        let sender = std::thread::spawn(move || {
            let req = crate::instance::Request { path: PathBuf::from(r"C:\x.md"), anchor: None, find: None };
            crate::instance::forward(&name, &id, &req, Instant::now() + Duration::from_secs(5))
        });
        entered.recv_timeout(Duration::from_secs(5)).expect("the commit started");
        w.set(false);
        assert!(w.app.server.is_none());
        assert!(w.app.stopped_server.as_ref().is_some_and(|s| !s.is_finished()), "still busy in commit");
        w.set(true);
        assert!(w.saved() && w.app.setting_on, "the box stays on");
        assert!(w.app.server.is_none(), "no new pipe while the old one is held");
        assert_eq!(w.status(), Some("Receiving starts with the next window"));
        // Once the old thread is through, turning it on again claims.
        release.send(()).unwrap();
        let _ = sender.join();
        assert!(crate::instance::pipe_tests::wait_finished(w.app.stopped_server.as_ref().unwrap()));
        w.set(true);
        assert!(w.app.server.is_some() && w.name_taken());
    }

    #[cfg(windows)]
    #[test]
    fn write_failure_sets_a_status_and_leaves_it_off() {
        let mut w = TestWindow::new("write-fails", Args::default());
        // Longer than the registry's 255-character limit for a key name.
        w.app.setting_subkey = Some(format!(r"Software\{}", "x".repeat(300)));
        w.set(true);
        assert!(w.status().is_some_and(|s| s.starts_with("Cannot save the setting: ")), "{:?}", w.status());
        assert!(!w.app.setting_on);
        assert!(w.app.server.is_none());
        assert!(!w.name_taken());
    }
}
