//! Application state, input handling and the top-level layout
//! (outline sidebar · reading column · file sidebar · status bar).

use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

use crate::document::{percent_decode, Block, Document, InlineItem};
use crate::files;
use crate::fonts::{self, FontSet};
use crate::highlight::{self, Highlighter};
use crate::render::{Action, RenderCtx};
use crate::search::SearchState;
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

#[derive(Default)]
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
}

#[derive(Clone, Copy, Default)]
struct Measure {
    height: f32,
    measured: bool,
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
    pending_offset: Option<f32>,
    scroll_delta: f32,
    jump: Option<(usize, u8)>,
    pending_anchor: Option<String>,

    search: SearchState,
    hl: Highlighter,
    hovered_link: Option<(usize, u32)>,

    /// Reading-column viewport from the last frame, in points.
    content_rect: Rect,
    menu: Option<ContextMenu>,
    /// Right-press position waiting for the next frame's hover info.
    menu_request: Option<Pos2>,
    /// A press was swallowed in `raw_input_hook`; swallow its release too.
    swallow_release: Option<PointerButton>,

    history: Vec<(PathBuf, f32)>,
    history_pos: usize,

    watcher: Option<notify::RecommendedWatcher>,
    watch_rx: Option<mpsc::Receiver<()>>,
    reload_at: Option<Instant>,
    dialog_rx: Option<mpsc::Receiver<Option<PathBuf>>>,

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
    screenshot_requested: bool,
}

/// Label for the primary shortcut modifier: egui's `Modifiers::COMMAND` is Cmd on macOS
/// and Ctrl everywhere else.
const CMD: &str = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" };

/// Shell command that prints the chosen file path on stdout, or nothing if cancelled.
/// macOS uses the native panel through `osascript`; elsewhere `zenity` (GTK).
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
            pending_offset: None,
            scroll_delta: 0.0,
            jump: None,
            pending_anchor: None,
            search: SearchState::default(),
            hl: Highlighter::new(ctx.clone()),
            hovered_link: None,
            content_rect: Rect::NOTHING,
            menu: None,
            menu_request: None,
            swallow_release: None,
            history: Vec::new(),
            history_pos: 0,
            watcher: None,
            watch_rx: None,
            reload_at: None,
            dialog_rx: None,
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
            screenshot_requested: false,
        };

        if args.stdin {
            let mut s = String::new();
            use std::io::Read;
            let _ = std::io::stdin().read_to_string(&mut s);
            app.stdin_mode = true;
            app.set_document(s, None, ctx);
        } else if let Some(p) = args.files.first() {
            app.load_path(p.clone(), true, ctx);
            app.pending_anchor = args.anchor.clone();
        }
        if let Some(q) = &args.find {
            app.search.open = true;
            app.search.query = q.clone();
            app.search.focus_pending = true;
        }
        app
    }

    // ------------------------------------------------------------------
    // Loading
    // ------------------------------------------------------------------

    fn set_document(&mut self, text: String, path: Option<PathBuf>, ctx: &Context) {
        let t0 = Instant::now();
        let doc = Document::parse(&text);
        log::debug!("parsed {} bytes into {} blocks in {:.2} ms", text.len(), doc.blocks.len(), t0.elapsed().as_secs_f64() * 1000.0);
        self.gen += 1;
        self.hl.retain_generation(self.gen);
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

    fn load_path(&mut self, path: PathBuf, push_history: bool, ctx: &Context) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if push_history {
                    if let Some(cur) = self.history.get_mut(self.history_pos) {
                        cur.1 = self.scroll_offset;
                    }
                    if !self.history.is_empty() {
                        self.history.truncate(self.history_pos + 1);
                    }
                    self.history.push((path.clone(), 0.0));
                    self.history_pos = self.history.len() - 1;
                }
                self.stdin_mode = false;
                self.set_document(text, Some(path.clone()), ctx);
                self.pending_offset = Some(0.0);
                self.jump = None;
                self.setup_watcher(&path, ctx);
                let dir = path.parent().map(Path::to_path_buf);
                if dir != self.files_dir {
                    self.files_dir = dir.clone();
                    self.files.clear();
                    if let Some(d) = dir {
                        self.files_rx = Some(files::spawn_scan(d, ctx.clone()));
                    }
                }
            }
            Err(e) => {
                self.error = Some(format!("Cannot read {}: {e}", path.display()));
                self.doc = None;
                self.path = Some(path);
                ctx.send_viewport_cmd(ViewportCommand::Title("mdread".into()));
            }
        }
    }

    fn reload(&mut self, ctx: &Context, announce: bool) {
        let Some(path) = self.path.clone() else { return };
        match std::fs::read(&path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
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
            Err(e) => self.set_status(format!("Reload failed: {e}")),
        }
    }

    fn setup_watcher(&mut self, path: &Path, ctx: &Context) {
        self.watcher = None;
        self.watch_rx = None;
        let Some(dir) = path.parent() else { return };
        let (tx, rx) = mpsc::channel();
        let target = path.to_path_buf();
        let ctx = ctx.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let relevant = ev.paths.is_empty() || ev.paths.iter().any(|p| p == &target);
                if relevant && tx.send(()).is_ok() {
                    ctx.request_repaint();
                }
            }
        });
        match watcher {
            Ok(mut w) => {
                if let Err(e) = w.watch(dir, notify::RecursiveMode::NonRecursive) {
                    log::warn!("watch {}: {e}", dir.display());
                } else {
                    self.watcher = Some(w);
                    self.watch_rx = Some(rx);
                }
            }
            Err(e) => log::warn!("file watcher unavailable: {e}"),
        }
    }

    fn open_dialog(&mut self, ctx: &Context) {
        if self.dialog_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let start_dir = self.files_dir.clone();
        std::thread::Builder::new()
            .name("file-dialog".into())
            .spawn(move || {
                let out = file_dialog_command(start_dir.as_deref()).output().ok();
                let path = out
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

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    // ------------------------------------------------------------------
    // Navigation
    // ------------------------------------------------------------------

    fn jump_to_block(&mut self, block: usize) {
        self.jump = Some((block, 6));
        self.pending_offset = None;
    }

    fn go_history(&mut self, delta: i32, ctx: &Context) {
        let n = self.history.len() as i32;
        let target = self.history_pos as i32 + delta;
        if n == 0 || target < 0 || target >= n {
            return;
        }
        if let Some(cur) = self.history.get_mut(self.history_pos) {
            cur.1 = self.scroll_offset;
        }
        self.history_pos = target as usize;
        let (path, offset) = self.history[self.history_pos].clone();
        self.load_path(path, false, ctx);
        self.pending_offset = Some(offset);
    }

    fn open_link(&mut self, url: &str, ctx: &Context) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        if let Some(anchor) = url.strip_prefix('#') {
            let target = self.doc.as_ref().and_then(|d| d.find_slug(anchor));
            match target {
                Some(b) => self.jump_to_block(b),
                None => self.set_status(format!("Anchor not found: #{anchor}")),
            }
            return;
        }
        if url.contains("://") || url.starts_with("mailto:") || url.starts_with("www.") {
            open_external(url);
            self.set_status(format!("Opened {url}"));
            return;
        }
        let (path_part, frag) = match url.split_once('#') {
            Some((p, f)) => (p, Some(f.to_owned())),
            None => (url, None),
        };
        let rel = percent_decode(path_part);
        let abs = self.resolve_local(&rel);
        if rel.is_empty() {
            if let Some(f) = frag {
                self.open_link(&format!("#{f}"), ctx);
            }
            return;
        }
        if abs.is_dir() {
            for name in ["README.md", "readme.md", "index.md", "README.markdown"] {
                let p = abs.join(name);
                if p.is_file() {
                    self.load_path(p, true, ctx);
                    self.pending_anchor = frag;
                    return;
                }
            }
            open_external(&abs.display().to_string());
            return;
        }
        if abs.is_file() {
            if files::is_markdown(&abs) || abs.extension().is_none() {
                self.load_path(abs, true, ctx);
                self.pending_anchor = frag;
            } else {
                open_external(&abs.display().to_string());
                self.set_status(format!("Opened {}", abs.display()));
            }
        } else {
            self.set_status(format!("Not found: {}", abs.display()));
        }
    }

    /// Absolute path for a link written relative to the current document,
    /// with `.` and `..` segments folded.
    fn resolve_local(&self, rel: &str) -> PathBuf {
        let joined = if Path::new(rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            let base = self.path.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
            base.join(rel)
        };
        let mut out = PathBuf::new();
        for c in joined.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !out.pop() && !out.has_root() {
                        out.push(c);
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    /// The link as it should read outside the viewer: web links and anchors as
    /// written, relative file links as absolute paths.
    fn link_for_clipboard(&self, url: &str) -> String {
        let url = url.trim();
        if url.starts_with('#') || url.contains("://") || url.starts_with("mailto:") || url.starts_with("www.") {
            return url.to_owned();
        }
        let (path_part, frag) = url.split_once('#').unwrap_or((url, ""));
        let rel = percent_decode(path_part);
        if rel.is_empty() {
            return url.to_owned();
        }
        let abs = self.resolve_local(&rel);
        if frag.is_empty() {
            abs.display().to_string()
        } else {
            format!("{}#{frag}", abs.display())
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
        let out = Popup::new(Id::new("context-menu"), ctx.clone(), PopupAnchor::Position(menu.pos), LayerId::background())
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
                    let target = local_path(uri);
                    open_external(target);
                    self.set_status(format!("Opened {target}"));
                }
            }
            MenuItem::CopyImage => {
                if let Some(uri) = &menu.image {
                    ctx.copy_text(local_path(uri).to_owned());
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

    fn zoom_to(&mut self, ctx: &Context, zoom: f32) {
        let zoom = zoom.clamp(0.5, 3.0);
        ctx.set_zoom_factor(zoom);
        self.settings.zoom = zoom;
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    fn handle_input(&mut self, ctx: &Context) {
        // Dropped files
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if let Some(p) = dropped.into_iter().next() {
            self.load_path(p, true, ctx);
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
            ZoomReset,
        }
        let mut cmds: Vec<Cmd> = Vec::new();

        ctx.input_mut(|i| {
            let zd = i.zoom_delta();
            if (zd - 1.0).abs() > 1e-4 {
                zoom = Some(cur_zoom * zd);
            }
            let cmd = Modifiers::COMMAND;
            if i.consume_key(cmd, Key::O) {
                cmds.push(Cmd::Open);
            }
            if i.consume_key(cmd, Key::Q) || i.consume_key(cmd, Key::W) {
                cmds.push(Cmd::Quit);
            }
            if i.consume_key(cmd, Key::F) {
                cmds.push(Cmd::Search);
            }
            if i.consume_key(cmd, Key::R) || i.consume_key(Modifiers::NONE, Key::F5) {
                cmds.push(Cmd::Reload);
            }
            if i.consume_key(Modifiers::ALT, Key::ArrowLeft) {
                cmds.push(Cmd::Back);
            }
            if i.consume_key(Modifiers::ALT, Key::ArrowRight) {
                cmds.push(Cmd::Forward);
            }
            if i.consume_key(Modifiers::NONE, Key::F3) {
                cmds.push(Cmd::Next);
            }
            if i.consume_key(Modifiers::SHIFT, Key::F3) {
                cmds.push(Cmd::Prev);
            }
            if i.consume_key(Modifiers::NONE, Key::F1) {
                cmds.push(Cmd::Help);
            }
            if i.key_pressed(Key::Escape) {
                cmds.push(Cmd::Escape);
            }
            if !typing {
                if i.consume_key(Modifiers::NONE, Key::J) || i.key_pressed(Key::ArrowDown) {
                    delta += line;
                }
                if i.consume_key(Modifiers::NONE, Key::K) || i.key_pressed(Key::ArrowUp) {
                    delta -= line;
                }
                if i.consume_key(Modifiers::NONE, Key::Space) || i.key_pressed(Key::PageDown) {
                    delta += page;
                }
                if i.consume_key(Modifiers::SHIFT, Key::Space) || i.key_pressed(Key::PageUp) {
                    delta -= page;
                }
                if i.consume_key(Modifiers::NONE, Key::D) {
                    delta += page * 0.5;
                }
                if i.consume_key(Modifiers::NONE, Key::U) {
                    delta -= page * 0.5;
                }
                if i.consume_key(Modifiers::NONE, Key::G) || i.key_pressed(Key::Home) {
                    cmds.push(Cmd::Top);
                }
                if i.consume_key(Modifiers::SHIFT, Key::G) || i.key_pressed(Key::End) {
                    cmds.push(Cmd::Bottom);
                }
                if i.consume_key(Modifiers::NONE, Key::Slash) {
                    cmds.push(Cmd::Search);
                }
                if i.consume_key(Modifiers::NONE, Key::N) {
                    cmds.push(Cmd::Next);
                }
                if i.consume_key(Modifiers::SHIFT, Key::N) {
                    cmds.push(Cmd::Prev);
                }
                if i.consume_key(Modifiers::NONE, Key::O) {
                    cmds.push(Cmd::Outline);
                }
                if i.consume_key(Modifiers::NONE, Key::F) {
                    cmds.push(Cmd::Files);
                }
                if i.consume_key(Modifiers::NONE, Key::T) {
                    cmds.push(Cmd::Theme);
                }
                if i.consume_key(Modifiers::NONE, Key::W) {
                    cmds.push(Cmd::Width);
                }
                if i.consume_key(Modifiers::NONE, Key::OpenBracket) {
                    cmds.push(Cmd::Narrower);
                }
                if i.consume_key(Modifiers::NONE, Key::CloseBracket) {
                    cmds.push(Cmd::Wider);
                }
                if i.consume_key(Modifiers::NONE, Key::Q) {
                    cmds.push(Cmd::Quit);
                }
                if i.consume_key(Modifiers::NONE, Key::R) {
                    cmds.push(Cmd::Reload);
                }
                if i.consume_key(Modifiers::SHIFT, Key::Questionmark) || i.consume_key(Modifiers::NONE, Key::Questionmark) {
                    cmds.push(Cmd::Help);
                }
                if i.consume_key(Modifiers::NONE, Key::Backspace) {
                    cmds.push(Cmd::Back);
                }
                if i.consume_key(Modifiers::NONE, Key::Plus) || i.consume_key(Modifiers::NONE, Key::Equals) || i.consume_key(Modifiers::SHIFT, Key::Equals) || i.consume_key(Modifiers::SHIFT, Key::Plus) {
                    zoom = Some(cur_zoom * 1.1);
                }
                if i.consume_key(Modifiers::NONE, Key::Minus) {
                    zoom = Some(cur_zoom / 1.1);
                }
                if i.consume_key(Modifiers::NONE, Key::Num0) {
                    cmds.push(Cmd::ZoomReset);
                }
            }
        });

        if delta != 0.0 {
            self.scroll_delta += delta;
        }
        if let Some(z) = zoom {
            self.zoom_to(ctx, z);
        }
        for c in cmds {
            match c {
                Cmd::Open => self.open_dialog(ctx),
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
                Cmd::Narrower => self.settings.column_width = (self.settings.column_width - 60.0).max(400.0),
                Cmd::Wider => self.settings.column_width = (self.settings.column_width + 60.0).min(3000.0),
                Cmd::Help => self.show_help = !self.show_help,
                Cmd::ZoomReset => self.zoom_to(ctx, 1.0),
            }
        }
    }

    fn poll_channels(&mut self, ctx: &Context) {
        if let Some(rx) = &self.watch_rx {
            let mut changed = false;
            while rx.try_recv().is_ok() {
                changed = true;
            }
            if changed {
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
        Panel::left(Id::new("outline"))
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
        if self.files.len() < 2 {
            return;
        }
        let Some(dir) = self.files_dir.clone() else { return };
        let base = self.settings.base_size;
        let pal = self.pal;
        let current = self.path.clone();
        let mut clicked: Option<PathBuf> = None;
        Panel::right(Id::new("files"))
            .resizable(true)
            .default_size(260.0)
            .size_range(160.0..=700.0)
            .frame(self.panel_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("FILES").size(base * 0.72).color(pal.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        ui.add(Label::new(RichText::new(name).size(base * 0.72).color(pal.text_dim)).truncate());
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
        Panel::top(Id::new("search"))
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
        Panel::bottom(Id::new("status"))
            .resizable(false)
            .show_separator_line(true)
            .frame(Frame::new().fill(pal.panel_bg).inner_margin(Margin::symmetric(12, 5)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let path_text = if self.stdin_mode {
                        "stdin".to_owned()
                    } else {
                        self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
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
                        let width_label = if self.settings.full_width { "full width".to_owned() } else { format!("{:.0}pt", self.settings.column_width) };
                        ui.label(RichText::new(width_label).size(small).color(pal.text_dim)).on_hover_text("Reading column width ([ ] to adjust, w for full width)");
                        ui.separator();
                        ui.label(RichText::new("? help").size(small).color(pal.text_dim));
                    });
                });
            });
    }

    fn show_help(&mut self, ctx: &Context) {
        if !self.show_help {
            return;
        }
        let pal = self.pal;
        let base = self.settings.base_size;
        let mut open = self.show_help;
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
                let rows: &[(&str, &str)] = &[
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
                    ("Alt+←  Backspace  /  Alt+→", "Back / forward"),
                    ("r  F5", "Reload (files also reload automatically)"),
                    ("Click link", "Open in browser, or follow to another .md"),
                    ("Hover code block", "Copy button"),
                    ("Right-click", "Copy selection, open or copy link"),
                    (&quit_key, "Quit"),
                ];
                egui::Grid::new("help-grid").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
                    for (k, v) in rows {
                        ui.label(RichText::new(*k).monospace().color(pal.accent));
                        ui.label(RichText::new(*v).color(pal.text));
                        ui.end_row();
                    }
                });
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!("Fonts: {} · {}", self.fonts.sans_name, self.fonts.mono_name))
                        .size(base * 0.8)
                        .color(pal.text_dim),
                );
            });
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
            sa = sa.vertical_scroll_offset(off.min(self.content_h.max(0.0)));
        }

        let scroll_delta = std::mem::take(&mut self.scroll_delta);
        let jump = self.jump;
        let measure_all = std::mem::take(&mut self.measure_all);
        let base_dir = self.path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
        let search = if self.search.open && !self.search.matches.is_empty() { Some(&self.search) } else { None };
        let hovered_link = self.hovered_link;

        let mut rc = RenderCtx::new(&pal, &self.fonts, base, base_dir.as_deref(), self.gen, &mut self.hl, search, hovered_link);
        let measures = &mut self.measures;
        let block_tops = &mut self.block_tops;
        let gap = rc.gap();

        let out = sa.show_viewport(ui, |ui, _viewport| {
            if scroll_delta != 0.0 {
                ui.scroll_with_delta(vec2(0.0, -scroll_delta));
            }
            let visible = ui.clip_rect().expand2(vec2(0.0, ui.clip_rect().height().max(400.0)));
            ui.add_space(base * 1.2);
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
                    if in_view || measure_all || is_target || !m.measured && i + 1 == n {
                        rc.show_block(ui, block);
                        if i + 1 < n {
                            ui.add_space(gap);
                        }
                        let h = ui.cursor().top() - top;
                        measures[i] = Measure { height: h, measured: true };
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
        self.scroll_offset = out.state.offset.y;
        self.viewport_h = out.inner_rect.height();
        self.content_h = out.content_size.y;

        if let Some((b, left)) = self.jump {
            if left == 0 {
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
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let [w, h] = img.size;
            match image::save_buffer(&path, img.as_raw(), w as u32, h as u32, image::ColorType::Rgba8) {
                Ok(()) => eprintln!("screenshot saved to {} ({w}x{h})", path.display()),
                Err(e) => eprintln!("screenshot failed: {e}"),
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
        } else {
            if !self.screenshot_requested && self.started.elapsed() >= Duration::from_millis(self.screenshot_delay_ms) {
                self.screenshot_requested = true;
                log::debug!("requesting screenshot at frame {}", self.frames);
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
            }
            // Keep frames coming so the Screenshot event is picked up.
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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frames += 1;
        if self.frames == 1 {
            log::info!("first frame {:.1} ms after launch", self.started.elapsed().as_secs_f64() * 1000.0);
        }
        let frame_t0 = Instant::now();
        self.poll_channels(&ctx);
        self.handle_input(&ctx);
        self.handle_screenshot(&ctx);

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
        self.show_context_menu(&ctx);
        self.show_help(&ctx);
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

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.pal.bg.to_normalized_gamma_f32()
    }
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

fn open_external(target: &str) {
    let t = target.to_owned();
    std::thread::Builder::new()
        .name("open".into())
        .spawn(move || {
            if let Err(e) = open::that_detached(&t) {
                log::warn!("open {t}: {e}");
            }
        })
        .ok();
}

#[allow(dead_code)]
fn _unused(_: Color32, _: Stroke) {}
