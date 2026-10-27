//! Where links and images point: a heading in the document, a URL for the OS, a local
//! file, or something that is refused. On Windows a click on a URL or a file goes to the
//! shell, which runs programs, scripts and any registered `scheme:` handler, and a path
//! like `\\host\share` makes an SMB connection that sends the user's credentials to that
//! host. So only web and mail URLs leave the app there, and network paths are only
//! followed on the share the document itself is on.

use std::path::{Path, PathBuf};

use crate::document::percent_decode;

#[derive(Debug, PartialEq)]
pub enum Target {
    /// `#slug` in the current document.
    Anchor(String),
    /// A URL for the OS to open.
    External(String),
    /// A local file or folder, with the `#fragment` after it.
    Local(PathBuf, Option<String>),
    /// Refused; the message says why.
    Blocked(String),
}

/// Classifies a link the user clicked. `base` is the folder relative links start from;
/// `None` refuses relative links.
pub fn classify_link(url: &str, base: Option<&Path>) -> Option<Target> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    if let Some(anchor) = url.strip_prefix('#') {
        return Some(Target::Anchor(anchor.to_owned()));
    }
    let (path_part, frag) = match url.split_once('#') {
        Some((p, f)) => (p, Some(f.to_owned())),
        None => (url, None),
    };
    let rel = match file_url(path_part) {
        // `file:` links open in the app like any other path (on Windows; elsewhere they
        // go to the OS as before).
        Some(rest) if cfg!(windows) => match file_url_path(rest) {
            Some(p) => p,
            None => return Some(Target::Blocked(format!("Blocked link to another computer: {url}"))),
        },
        _ => match external(url) {
            Some(t) => return Some(t),
            None => percent_decode(path_part),
        },
    };
    if rel.is_empty() {
        return frag.map(Target::Anchor);
    }
    Some(match resolve_local(&rel, base) {
        Ok(p) => Target::Local(p, frag),
        Err(msg) => Target::Blocked(msg),
    })
}

/// Links that leave the app, or are refused because of their scheme.
fn external(url: &str) -> Option<Target> {
    if !cfg!(windows) {
        let ext = url.contains("://") || url.starts_with("mailto:") || url.starts_with("www.");
        return ext.then(|| Target::External(url.to_owned()));
    }
    if url.starts_with("www.") {
        return Some(Target::External(format!("https://{url}")));
    }
    let scheme = scheme(url)?;
    Some(if ["http", "https", "mailto"].iter().any(|s| s.eq_ignore_ascii_case(scheme)) {
        Target::External(url.to_owned())
    } else {
        Target::Blocked(format!("Blocked {scheme}: link"))
    })
}

/// The URL scheme `s` starts with, if any. One letter is a drive (`C:\x`), not a scheme.
fn scheme(s: &str) -> Option<&str> {
    let (scheme, _) = s.split_once(':')?;
    let mut chars = scheme.chars();
    let valid = scheme.len() > 1
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

/// What follows `file:` in a `file:` URL.
fn file_url(s: &str) -> Option<&str> {
    scheme(s).filter(|sc| sc.eq_ignore_ascii_case("file")).map(|sc| &s[sc.len() + 1..])
}

/// The percent-decoded path of a `file:` URL (the part after `file:`), or `None` when it
/// names another host (`file://host/share/x`, a UNC path on Windows).
fn file_url_path(rest: &str) -> Option<String> {
    let rest = rest.split('?').next().unwrap_or(rest);
    let path = match rest.strip_prefix("//") {
        Some(auth) => {
            let (host, path) = auth.split_at(auth.find('/').unwrap_or(auth.len()));
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return None;
            }
            path
        }
        None => rest,
    };
    Some(percent_decode(path))
}

/// Resolves a decoded local path against `base`. `Err` holds the status message.
pub fn resolve_local(rel: &str, base: Option<&Path>) -> Result<PathBuf, String> {
    // `/C:/x` is how URL-minded tools write `C:/x`.
    #[cfg(windows)]
    let rel = match rel.as_bytes() {
        [b'/', d, b':', rest @ ..] if d.is_ascii_alphabetic() && matches!(rest.first(), None | Some(b'/' | b'\\')) => &rel[1..],
        _ => rel,
    };
    let p = Path::new(rel);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        // `C:x` is relative to the process's current folder on drive C:.
        if cfg!(windows) && matches!(p.components().next(), Some(std::path::Component::Prefix(_))) {
            return Err(format!("Unsupported path: {rel}"));
        }
        match base {
            Some(b) => b.join(p),
            None => return Err(format!("No folder to resolve {rel} against")),
        }
    };
    if !network_allowed(&abs, base) {
        return Err(format!("Blocked network path: {}", display_path(&abs)));
    }
    Ok(abs)
}

/// The `file://` URI for a local image, or `None` if `url` is not one or is refused.
/// Windows only; see [`path_to_file_uri`].
#[cfg(windows)]
pub fn local_image_uri(url: &str, base: Option<&Path>) -> Option<String> {
    let path_part = url.split(['#', '?']).next().unwrap_or(url);
    let rel = match (file_url(path_part), scheme(path_part)) {
        (Some(rest), _) => file_url_path(rest)?,
        (None, Some(_)) => return None,
        (None, None) => percent_decode(path_part),
    };
    if rel.is_empty() {
        return None;
    }
    match resolve_local(&rel, base) {
        Ok(p) => Some(path_to_file_uri(&p)),
        Err(msg) => {
            // Runs every frame the image is visible.
            log::trace!("image {url}: {msg}");
            None
        }
    }
}

/// A `file://` URI that egui_extras' file loader turns back into `p`. It strips
/// `file://`, then reads `/rest` as the path `rest` and anything else as `\\rest` (UNC),
/// and does no percent-decoding, so neither does this encode anything.
#[cfg(windows)]
pub fn path_to_file_uri(p: &Path) -> String {
    let s = p.as_os_str().to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        format!("file://{unc}")
    } else if let Some(unc) = s.strip_prefix(r"\\").filter(|_| !s.starts_with(r"\\?\")) {
        format!("file://{unc}")
    } else {
        // `C:\x`, or `\\?\C:\x` for paths too long to lose the prefix.
        format!("file:///{s}")
    }
}

/// The inverse of [`path_to_file_uri`], the same way egui_extras reads it.
#[cfg(windows)]
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let s = uri.strip_prefix("file://")?;
    Some(match s.strip_prefix('/') {
        Some(local) => PathBuf::from(local),
        None => PathBuf::from(format!(r"\\{s}")),
    })
}

/// Network paths (UNC, `\\.\` devices, other `\\?\` forms) are only allowed on the share
/// that `base`, the document's folder, is on.
#[cfg(windows)]
fn network_allowed(p: &Path, base: Option<&Path>) -> bool {
    use std::path::{Component, Prefix};
    fn prefix(p: &Path) -> Option<Prefix<'_>> {
        match p.components().next() {
            Some(Component::Prefix(pc)) => Some(pc.kind()),
            _ => None,
        }
    }
    let share = |p: &Path| match prefix(p) {
        Some(Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share)) => {
            Some(format!(r"{}\{}", server.to_string_lossy(), share.to_string_lossy()).to_lowercase())
        }
        _ => None,
    };
    match prefix(p) {
        None | Some(Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => true,
        Some(_) => share(p).is_some_and(|s| base.and_then(share) == Some(s)),
    }
}

#[cfg(not(windows))]
fn network_allowed(_: &Path, _: Option<&Path>) -> bool {
    true
}

/// Files a click opens in their default app on Windows: images, PDFs, plain text and
/// media. Anything else could be a program, a script or a shortcut to one, so it is only
/// shown in Explorer. Other platforms open everything, as before.
pub fn opens_in_default_app(p: &Path) -> bool {
    const SAFE: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico", "avif", "heic", "pdf", "txt", "text", "log",
        "csv", "tsv", "json", "yaml", "yml", "toml", "mp3", "wav", "flac", "ogg", "m4a", "mp4", "m4v", "mkv", "webm",
        "mov", "avi",
    ];
    if !cfg!(windows) {
        return true;
    }
    // `x.exe:stream.pdf` names an alternate data stream of x.exe.
    let name = p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    !name.contains(':') && p.extension().and_then(|e| e.to_str()).is_some_and(|e| SAFE.iter().any(|s| s.eq_ignore_ascii_case(e)))
}

/// A path for people to read: without the `\\?\` prefix Windows uses for long paths.
pub fn display_path(p: &Path) -> String {
    let s = p.display().to_string();
    if cfg!(windows) {
        if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{unc}");
        }
        if let Some(local) = s.strip_prefix(r"\\?\") {
            return local.to_owned();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes_and_drive_letters() {
        assert_eq!(scheme("https://x.org"), Some("https"));
        assert_eq!(scheme("ms-settings:display"), Some("ms-settings"));
        assert_eq!(scheme("C:/notes/a.md"), None);
        assert_eq!(scheme("notes/a.md"), None);
        assert_eq!(scheme("a b:c"), None);
    }

    #[test]
    fn file_url_paths() {
        assert_eq!(file_url_path("///C:/My%20Docs/a.md"), Some("/C:/My Docs/a.md".into()));
        assert_eq!(file_url_path("//localhost/home/a.md"), Some("/home/a.md".into()));
        assert_eq!(file_url_path("/home/a.md?x=1"), Some("/home/a.md".into()));
        assert_eq!(file_url_path("//host/share/a.md"), None);
    }

    #[test]
    fn anchors_and_fragments() {
        let base = Path::new("docs");
        assert_eq!(classify_link("#intro", Some(base)), Some(Target::Anchor("intro".into())));
        assert_eq!(classify_link("  ", Some(base)), None);
        assert_eq!(classify_link("b.md#x", Some(base)), Some(Target::Local(base.join("b.md"), Some("x".into()))));
        assert_eq!(classify_link("a%20b.md", Some(base)), Some(Target::Local(base.join("a b.md"), None)));
        assert!(matches!(classify_link("b.md", None), Some(Target::Blocked(_))));
    }

    #[test]
    fn web_links_leave_the_app() {
        let t = classify_link("https://example.org/a#b", None);
        assert_eq!(t, Some(Target::External("https://example.org/a#b".into())));
        assert_eq!(classify_link("mailto:a@b.c", None), Some(Target::External("mailto:a@b.c".into())));
    }

    #[cfg(windows)]
    mod windows {
        use super::super::*;

        fn base() -> &'static Path {
            Path::new(r"C:\docs")
        }

        #[test]
        fn only_web_and_mail_schemes_leave_the_app() {
            assert_eq!(classify_link("www.example.org", None), Some(Target::External("https://www.example.org".into())));
            for url in ["ms-settings:display", "search-ms:query=x", "ms-officecmd://x", "javascript:alert(1)", "ftp://x/y"] {
                assert!(matches!(classify_link(url, Some(base())), Some(Target::Blocked(_))), "{url}");
            }
        }

        #[test]
        fn file_urls_and_slash_drive_paths_resolve_locally() {
            let want = Some(Target::Local(PathBuf::from("C:/My Docs/b.md"), Some("s".into())));
            assert_eq!(classify_link("file:///C:/My%20Docs/b.md#s", Some(base())), want);
            assert_eq!(classify_link("FILE://localhost/C:/My%20Docs/b.md#s", Some(base())), want);
            assert_eq!(classify_link("/C:/My Docs/b.md#s", Some(base())), want);
            assert_eq!(classify_link(r"sub\b.md", Some(base())), Some(Target::Local(PathBuf::from(r"C:\docs\sub\b.md"), None)));
        }

        #[test]
        fn network_paths_are_blocked_unless_on_the_documents_share() {
            for url in ["//evil/share/a.md", r"\\evil\share\a.md", "file://evil/share/a.md", r"\\.\pipe\x", "C:relative.md"] {
                assert!(matches!(classify_link(url, Some(base())), Some(Target::Blocked(_))), "{url}");
            }
            let share = Path::new(r"\\Server\Docs\guide");
            assert_eq!(classify_link("../b.md", Some(share)), Some(Target::Local(share.join("../b.md"), None)));
            assert!(matches!(classify_link(r"\\server\docs\b.md", Some(share)), Some(Target::Local(..))));
            assert!(matches!(classify_link(r"\\server\other\b.md", Some(share)), Some(Target::Blocked(_))));
            let verbatim = Path::new(r"\\?\UNC\server\docs\guide");
            assert!(matches!(classify_link("b.md", Some(verbatim)), Some(Target::Local(..))));
        }

        #[test]
        fn image_uris_round_trip_through_the_egui_file_loader() {
            for p in [r"C:\docs\img\a b.png", r"\\?\C:\docs\img\x.png", r"\\server\share\x.png", r"\\?\UNC\server\share\x.png"] {
                let uri = path_to_file_uri(Path::new(p));
                let back = file_uri_to_path(&uri).unwrap();
                let same = back == Path::new(p) || (p.starts_with(r"\\?\UNC\") && back == Path::new(r"\\server\share\x.png"));
                assert!(same, "{p} -> {uri} -> {back:?}");
            }
            assert_eq!(local_image_uri("img/a%20b.png?raw=1", Some(base())).as_deref(), Some(r"file:///C:\docs\img/a b.png"));
            assert_eq!(local_image_uri("file:///D:/pics/x.png", Some(base())).as_deref(), Some("file:///D:/pics/x.png"));
            assert_eq!(local_image_uri("//evil/share/x.png", Some(base())), None);
            assert_eq!(local_image_uri("file://evil/share/x.png", Some(base())), None);
            assert_eq!(local_image_uri("img/x.png", None), None);
        }

        #[test]
        fn only_safe_types_open_directly() {
            for name in ["a.PNG", "b.pdf", "c.txt", "d.csv"] {
                assert!(opens_in_default_app(Path::new(name)), "{name}");
            }
            for name in ["setup.bat", "run.exe", "x.lnk", "x.url", "s.ps1", "noext", "x.pdf.", "x.exe:s.pdf", "page.html"] {
                assert!(!opens_in_default_app(Path::new(name)), "{name}");
            }
        }

        #[test]
        fn display_strips_verbatim_prefixes() {
            assert_eq!(display_path(Path::new(r"\\?\C:\a.md")), r"C:\a.md");
            assert_eq!(display_path(Path::new(r"\\?\UNC\s\sh\a.md")), r"\\s\sh\a.md");
            assert_eq!(display_path(Path::new(r"C:\a.md")), r"C:\a.md");
        }
    }
}
