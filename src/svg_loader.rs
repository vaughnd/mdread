//! SVG loader for Windows: egui_extras' own one, except for `<image href>` inside the SVG.
//! usvg's default resolver reads any path an SVG names, relative to the process's current
//! folder (often System32 when started from Explorer), and `//host/share/x.png` makes an
//! SMB connection that sends the user's credentials to that host. Here a referenced image
//! must be a relative path, resolved against the SVG's own folder (or a `data:` URL).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use egui::load::{BytesPoll, ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use egui::mutex::Mutex;
use egui::ColorImage;
use resvg::usvg::{ImageHrefResolver, Options};

struct Entry {
    last_used: u64,
    result: Result<Arc<ColorImage>, String>,
}

struct State {
    pass_index: u64,
    cache: HashMap<String, HashMap<SizeHint, Entry>>,
    options: Options<'static>,
}

pub struct SvgLoader {
    state: Mutex<State>,
}

/// Installs the loader. Call before `egui_extras::install_image_loaders`: it uses
/// egui_extras' loader id, so that one is then not installed.
pub fn install(ctx: &egui::Context) {
    ctx.add_image_loader(Arc::new(SvgLoader::default()));
}

impl Default for SvgLoader {
    fn default() -> Self {
        let mut options = Options::default();
        let default_resolver = ImageHrefResolver::default_string_resolver();
        options.image_href_resolver.resolve_string = Box::new(move |href: &str, opts: &Options| {
            let p = Path::new(href);
            // A ':' is a scheme, a drive or a stream; a root or prefix leaves the SVG's folder.
            if href.contains(':') || p.has_root() || opts.resources_dir.is_none() {
                log::debug!("svg: ignoring <image href=\"{href}\">");
                return None;
            }
            default_resolver(href, opts)
        });
        Self { state: Mutex::new(State { pass_index: 0, cache: HashMap::new(), options }) }
    }
}

impl ImageLoader for SvgLoader {
    fn id(&self) -> &str {
        egui_extras::loaders::svg_loader::SvgLoader::ID
    }

    fn load(&self, ctx: &egui::Context, uri: &str, size_hint: SizeHint) -> ImageLoadResult {
        if !uri.ends_with(".svg") {
            return Err(LoadError::NotSupported);
        }
        let mut state = self.state.lock();
        let State { pass_index, cache, options } = &mut *state;
        let bucket = cache.entry(uri.to_owned()).or_default();
        if let Some(entry) = bucket.get_mut(&size_hint) {
            entry.last_used = *pass_index;
            return match entry.result.clone() {
                Ok(image) => Ok(ImagePoll::Ready { image }),
                Err(err) => Err(LoadError::Loading(err)),
            };
        }
        match ctx.try_load_bytes(uri) {
            Ok(BytesPoll::Ready { bytes, .. }) => {
                // Only local SVGs may reference files, from their own folder.
                options.resources_dir = crate::links::file_uri_to_path(uri).and_then(|p| p.parent().map(Path::to_path_buf));
                let result = egui_extras::image::load_svg_bytes_with_size(&bytes, size_hint, options).map(Arc::new);
                bucket.insert(size_hint, Entry { last_used: *pass_index, result: result.clone() });
                match result {
                    Ok(image) => Ok(ImagePoll::Ready { image }),
                    Err(err) => Err(LoadError::Loading(err)),
                }
            }
            Ok(BytesPoll::Pending { size }) => Ok(ImagePoll::Pending { size }),
            Err(err) => Err(err),
        }
    }

    fn forget(&self, uri: &str) {
        self.state.lock().cache.retain(|key, _| key != uri);
    }

    fn forget_all(&self) {
        self.state.lock().cache.clear();
    }

    fn byte_size(&self) -> usize {
        self.state
            .lock()
            .cache
            .values()
            .flat_map(|bucket| bucket.values())
            .map(|entry| match &entry.result {
                Ok(image) => image.pixels.len() * size_of::<egui::Color32>(),
                Err(err) => err.len(),
            })
            .sum()
    }

    fn end_pass(&self, pass_index: u64) {
        let mut state = self.state.lock();
        state.pass_index = pass_index;
        // Drop sizes of an SVG that were not drawn in the last pass (e.g. while resizing).
        state.cache.retain(|_, bucket| {
            if bucket.len() >= 2 {
                bucket.retain(|_, e| pass_index <= e.last_used + 1);
            }
            !bucket.is_empty()
        });
    }
}
