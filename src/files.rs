//! Find sibling Markdown files for the file sidebar.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

const EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "mdx"];
const SKIP_DIRS: &[&str] = &["node_modules", "target", ".git", "vendor", "dist", "build", "__pycache__", ".venv", "venv"];
const MAX_FILES: usize = 2000;
const MAX_DEPTH: usize = 4;

pub fn is_markdown(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

pub fn scan(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, 0, &mut out);
    out.sort_by(|a, b| {
        let da = a.components().count();
        let db = b.components().count();
        da.cmp(&db).then_with(|| a.to_string_lossy().to_lowercase().cmp(&b.to_string_lossy().to_lowercase()))
    });
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = e.file_type() else { continue };
        if hidden(&e) {
            continue;
        }
        if ft.is_dir() {
            if depth + 1 < MAX_DEPTH && !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref()) {
                walk(&path, depth + 1, out);
            }
        } else if (ft.is_file() || ft.is_symlink()) && is_markdown(&path) {
            out.push(path);
            if out.len() >= MAX_FILES {
                return;
            }
        }
    }
}

/// Windows marks hidden folders (AppData, $RECYCLE.BIN, System Volume Information) with
/// attributes rather than a leading dot. The attributes come with the directory listing,
/// so this costs no extra system call.
#[cfg(windows)]
fn hidden(e: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};
    e.metadata().is_ok_and(|m| m.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0)
}

#[cfg(not(windows))]
fn hidden(_: &std::fs::DirEntry) -> bool {
    false
}

pub fn spawn_scan(dir: PathBuf, ctx: egui::Context) -> mpsc::Receiver<Vec<PathBuf>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("file-scan".into())
        .spawn(move || {
            let files = scan(&dir);
            let _ = tx.send(files);
            ctx.request_repaint();
        })
        .ok();
    rx
}
