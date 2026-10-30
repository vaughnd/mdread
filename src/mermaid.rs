//! Mermaid diagrams (```` ```mermaid ```` code blocks). On a worker thread, merman lays a
//! diagram out as SVG and usvg turns that into a tree, which is then rasterised at the size
//! it is shown, so diagrams stay sharp at any zoom. Results are cached by source text and
//! theme, so a reload only redraws the diagrams that changed.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{vec2, ColorImage, TextureHandle, TextureId, TextureOptions, Vec2};
use merman::svg::{RootBackgroundPostprocessor, SvgPipeline};
use merman::{Engine, MermaidConfig, OperationControl, RenderError, RenderOutput, RenderRequest, Renderer, SvgRequest};
use resvg::tiny_skia;
use resvg::usvg::{self, fontdb};

/// Margin around a diagram, in diagram pixels, so glyphs that overhang the SVG's view box
/// (the actor names under a sequence diagram) are not cut off.
const PAD: f32 = 6.0;
/// Textures of diagrams that are not on screen are dropped above this total, and drawn
/// again when scrolled back to.
const TEXTURE_BUDGET: usize = 256 << 20;
/// A diagram that takes longer than this to lay out is given up on, so that the ones after
/// it are still drawn.
const LAYOUT_DEADLINE: Duration = Duration::from_secs(10);

/// Whether a fenced code block's info string asks for a Mermaid diagram.
pub fn is_mermaid(lang: Option<&str>) -> bool {
    lang.and_then(|l| l.split(|c: char| c.is_whitespace() || c == '{').next())
        .is_some_and(|l| l.eq_ignore_ascii_case("mermaid"))
}

type Key = (u64, bool);

/// A laid-out diagram, for [`Diagrams::texture`].
#[derive(Clone, Copy)]
pub struct Diagram {
    key: Key,
    /// In diagram pixels, margin included.
    pub size: Vec2,
}

pub enum Status {
    Pending,
    Failed(String),
    Ready(Diagram),
}

enum State {
    Pending,
    Failed(String),
    Ready { tree: Arc<usvg::Tree>, texture: Option<TextureHandle>, rastering: bool },
}

struct Entry {
    state: State,
    /// Last document generation that showed it.
    gen: u64,
    /// Last frame it was drawn in.
    drawn: u64,
}

enum Job {
    Layout { key: Key, source: Arc<str> },
    Raster { key: Key, tree: Arc<usvg::Tree>, size: [usize; 2] },
}

enum Done {
    Layout { key: Key, result: Result<Arc<usvg::Tree>, String> },
    Raster { key: Key, image: ColorImage },
}

pub struct Diagrams {
    ctx: egui::Context,
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Done>,
    entries: HashMap<Key, Entry>,
    frame: u64,
}

impl Diagrams {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, worker_rx) = mpsc::channel();
        let (worker_tx, rx) = mpsc::channel();
        let worker_ctx = ctx.clone();
        std::thread::Builder::new()
            .name("mermaid".into())
            .spawn(move || worker(worker_rx, worker_tx, worker_ctx))
            .expect("spawn mermaid thread");
        Self { ctx, tx, rx, entries: HashMap::new(), frame: 0 }
    }

    /// Takes in finished work. Call once per frame, before drawing.
    pub fn poll(&mut self) {
        self.frame += 1;
        while let Ok(done) = self.rx.try_recv() {
            match done {
                Done::Layout { key, result } => {
                    if let Some(e) = self.entries.get_mut(&key) {
                        e.state = match result {
                            Ok(tree) => State::Ready { tree, texture: None, rastering: false },
                            Err(err) => State::Failed(err),
                        };
                    }
                }
                Done::Raster { key, image } => {
                    if let Some(Entry { state: State::Ready { texture, rastering, .. }, .. }) = self.entries.get_mut(&key) {
                        *rastering = false;
                        match texture {
                            Some(t) => t.set(image, TextureOptions::LINEAR),
                            None => *texture = Some(self.ctx.load_texture("mermaid", image, TextureOptions::LINEAR)),
                        }
                    }
                }
            }
        }
        self.evict();
    }

    /// Drops the textures of diagrams not drawn in the last frame, least recently drawn
    /// first, while the total is over budget.
    fn evict(&mut self) {
        let bytes = |e: &Entry| match &e.state {
            State::Ready { texture: Some(t), .. } => t.byte_size(),
            _ => 0,
        };
        let mut total: usize = self.entries.values().map(bytes).sum();
        if total <= TEXTURE_BUDGET {
            return;
        }
        let mut idle: Vec<(u64, Key)> =
            self.entries.iter().filter(|(_, e)| e.drawn + 1 < self.frame && bytes(e) > 0).map(|(k, e)| (e.drawn, *k)).collect();
        idle.sort_unstable();
        for (_, key) in idle {
            if total <= TEXTURE_BUDGET {
                break;
            }
            if let Some(Entry { state: State::Ready { texture, .. }, .. }) = self.entries.get_mut(&key) {
                if let Some(t) = texture.take() {
                    total -= t.byte_size();
                }
            }
        }
    }

    /// The diagram for `source`; lays it out in the background the first time.
    pub fn status(&mut self, gen: u64, source: &Arc<str>, dark: bool) -> Status {
        let mut h = DefaultHasher::new();
        source.hash(&mut h);
        let key = (h.finish(), dark);
        let entry = self.entries.entry(key).or_insert_with(|| {
            let _ = self.tx.send(Job::Layout { key, source: source.clone() });
            Entry { state: State::Pending, gen, drawn: 0 }
        });
        entry.gen = gen;
        match &entry.state {
            State::Pending => Status::Pending,
            State::Failed(err) => Status::Failed(err.clone()),
            State::Ready { tree, .. } => {
                let size = vec2(tree.size().width(), tree.size().height()) + Vec2::splat(2.0 * PAD);
                Status::Ready(Diagram { key, size })
            }
        }
    }

    /// The texture to draw `d` with. `px` is the size wanted in physical pixels; until that
    /// is ready the texture has another size (or there is none yet), so the caller draws it
    /// into its rect. `None` means the diagram is far from view and nothing new is drawn.
    pub fn texture(&mut self, d: Diagram, px: Option<[usize; 2]>) -> Option<TextureId> {
        let entry = self.entries.get_mut(&d.key)?;
        let State::Ready { tree, texture, rastering } = &mut entry.state else { return None };
        if let Some(px) = px {
            entry.drawn = self.frame;
            if !*rastering && texture.as_ref().is_none_or(|t| t.size() != px) {
                *rastering = true;
                let _ = self.tx.send(Job::Raster { key: d.key, tree: tree.clone(), size: px });
            }
        }
        texture.as_ref().map(TextureHandle::id)
    }

    /// Whether any diagram is still being laid out, so its height is not known yet.
    pub fn laying_out(&self) -> bool {
        self.entries.values().any(|e| matches!(e.state, State::Pending))
    }

    /// Forgets diagrams that neither this document nor the one before it showed, so a
    /// reload keeps the ones that did not change.
    pub fn retain_generation(&mut self, gen: u64) {
        self.entries.retain(|_, e| e.gen + 1 >= gen);
    }
}

fn worker(rx: mpsc::Receiver<Job>, tx: mpsc::Sender<Done>, ctx: egui::Context) {
    let mut renderers: [Option<Renderer>; 2] = [None, None];
    let mut fonts: Option<Fonts> = None;
    let mut queue: Vec<Job> = Vec::new();
    loop {
        if queue.is_empty() {
            match rx.recv() {
                Ok(job) => queue.push(job),
                Err(_) => return,
            }
        }
        queue.extend(rx.try_iter());
        // Redraws first: they are for diagrams on screen, while layouts are requested for
        // the whole document so that its height is known.
        let next = queue.iter().position(|j| matches!(j, Job::Raster { .. })).unwrap_or(0);
        let done = match queue.remove(next) {
            Job::Layout { key, source } => {
                let t0 = Instant::now();
                let dark = key.1;
                let renderer = renderers[dark as usize].get_or_insert_with(|| {
                    let theme = if dark { "dark" } else { "default" };
                    // Mermaid 12's "neo" look strokes shapes with a gradient, which leaves out
                    // straight lines such as the compartment lines of class diagrams.
                    let config = MermaidConfig::from_value(serde_json::json!({ "theme": theme, "look": "classic" }));
                    Renderer::new().with_engine(Engine::new().with_site_config(config))
                });
                let fonts = fonts.get_or_insert_with(Fonts::load);
                let result = match lay_out(renderer, &source) {
                    Ok(Some(svg)) => fonts.parse(&svg).map(Arc::new).map_err(|e| format!("cannot read the SVG: {e}")),
                    Ok(None) => Err("not a Mermaid diagram".into()),
                    Err(RenderError::Cancelled(_)) => Err("it took too long to lay out".into()),
                    Err(e) => Err(e.to_string().lines().next().unwrap_or_default().to_owned()),
                };
                log::debug!("laid out a Mermaid diagram in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
                Done::Layout { key, result }
            }
            Job::Raster { key, tree, size } => Done::Raster { key, image: rasterize(&tree, size) },
        };
        if tx.send(done).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// The diagram as SVG that usvg can draw: labels as SVG text rather than HTML, and no
/// background, so that it sits on the page in either theme.
fn lay_out(renderer: &Renderer, source: &str) -> Result<Option<String>, RenderError> {
    let request = SvgRequest {
        pipeline: Some(SvgPipeline::resvg_safe().with_postprocessor(RootBackgroundPostprocessor::new("transparent"))),
        ..Default::default()
    };
    let control = OperationControl::new().with_deadline(LAYOUT_DEADLINE);
    match renderer.render(RenderRequest::svg(source, control, request))? {
        // Class, state and ER diagrams put each label in an <svg> of the width merman measured,
        // which can be less than the font draws, and that <svg> would cut the text off.
        RenderOutput::Svg(Some(svg)) => Ok(Some(svg.svg().replace(r#"style="overflow:hidden;"#, r#"style="overflow:visible;"#))),
        _ => Ok(None),
    }
}

fn rasterize(tree: &usvg::Tree, [w, h]: [usize; 2]) -> ColorImage {
    let (w, h) = (w.max(1), h.max(1));
    let Some(mut pixmap) = tiny_skia::Pixmap::new(w as u32, h as u32) else {
        return ColorImage::filled([w, h], egui::Color32::TRANSPARENT);
    };
    let sx = w as f32 / (tree.size().width() + 2.0 * PAD);
    let sy = h as f32 / (tree.size().height() + 2.0 * PAD);
    resvg::render(tree, tiny_skia::Transform::from_row(sx, 0.0, 0.0, sy, PAD * sx, PAD * sy), &mut pixmap.as_mut());
    ColorImage::from_rgba_premultiplied([w, h], pixmap.data())
}

/// Fonts for diagram text. merman sizes labels with the metrics of Trebuchet MS, Mermaid's
/// default font, so that is used where installed (Windows, macOS, Linux with the Microsoft
/// core fonts), else a sans-serif of similar width. usvg only searches the few fonts loaded
/// here, which keeps text layout fast (all system fonts make it 50 times slower); a font
/// for a character these lack is loaded the first time one is needed.
struct Fonts {
    db: Arc<fontdb::Database>,
    /// Every installed font, of which only the names have been read.
    system: fontdb::Database,
    /// Characters that no installed font has.
    missing: Mutex<HashSet<char>>,
}

impl Fonts {
    fn load() -> Self {
        let t0 = Instant::now();
        let mut system = fontdb::Database::new();
        system.load_system_fonts();
        let mut db = fontdb::Database::new();
        let sans = add_family(
            &system,
            &mut db,
            &["Trebuchet MS", "Arial", "Liberation Sans", "Helvetica", "Arimo", "Noto Sans", "DejaVu Sans"],
        );
        let mono = add_family(
            &system,
            &mut db,
            &["Courier New", "Liberation Mono", "Cousine", "Menlo", "Consolas", "DejaVu Sans Mono", "Noto Sans Mono"],
        );
        // Mermaid's stack ("trebuchet ms", verdana, arial, sans-serif) ends up at sans-serif,
        // as fontdb compares names case-sensitively; usvg falls back to serif after that.
        if let Some(sans) = &sans {
            db.set_sans_serif_family(sans.as_str());
            db.set_serif_family(sans.as_str());
        }
        if let Some(mono) = &mono {
            db.set_monospace_family(mono.as_str());
        }
        log::debug!(
            "diagram fonts {sans:?} and {mono:?} from {} system fonts in {:.1} ms",
            system.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
        Self { db: Arc::new(db), system, missing: Mutex::new(HashSet::new()) }
    }

    fn parse(&mut self, svg: &str) -> Result<usvg::Tree, usvg::Error> {
        let (system, missing) = (&self.system, &self.missing);
        let loaded = usvg::FontResolver::default_fallback_selector();
        let mut opt = usvg::Options { fontdb: self.db.clone(), ..Default::default() };
        // Diagrams are shapes and text. Never read an image an SVG names: a UNC path would
        // connect to that host.
        opt.image_href_resolver.resolve_string = Box::new(|_, _| None);
        opt.font_resolver.select_fallback =
            Box::new(move |c, used, db| loaded(c, used, db).or_else(|| load_fallback(system, missing, c, db)));
        let tree = usvg::Tree::from_str(svg, &opt)?;
        // Keep the fallback fonts loaded for this diagram for the next ones.
        self.db = tree.fontdb().clone();
        Ok(tree)
    }
}

/// Loads the first of `names` that is installed into `db`, all its styles, and returns its name.
fn add_family(system: &fontdb::Database, db: &mut fontdb::Database, names: &[&str]) -> Option<String> {
    for name in names {
        let mut found = None;
        let mut files = HashSet::new();
        for face in system.faces() {
            let Some((family, _)) = face.families.iter().find(|(f, _)| f.eq_ignore_ascii_case(name)) else { continue };
            if let Some(path) = face_path(face) {
                if files.insert(path) && db.load_font_file(path).is_ok() {
                    found = Some(family.clone());
                }
            }
        }
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Finds an installed font that has `c`, preferring a regular face, and adds it to `db`.
fn load_fallback(
    system: &fontdb::Database,
    missing: &Mutex<HashSet<char>>,
    c: char,
    db: &mut Arc<fontdb::Database>,
) -> Option<fontdb::ID> {
    let mut missing = missing.lock().ok()?;
    if missing.contains(&c) {
        return None;
    }
    let regular = |f: &&fontdb::FaceInfo| f.style == fontdb::Style::Normal && f.weight == fontdb::Weight::NORMAL;
    let has = |f: &&fontdb::FaceInfo| {
        let glyph = system.with_face_data(f.id, |data, index| ttf_parser::Face::parse(data, index).ok()?.glyph_index(c));
        matches!(glyph, Some(Some(_)))
    };
    let Some(face) = system.faces().filter(regular).find(has).or_else(|| system.faces().find(has)) else {
        missing.insert(c);
        return None;
    };
    let db = Arc::make_mut(db);
    let before: HashSet<fontdb::ID> = db.faces().map(|f| f.id).collect();
    db.load_font_file(face_path(face)?).ok()?;
    log::debug!("diagram text: {c:?} from {}", face.post_script_name);
    db.faces().find(|f| !before.contains(&f.id) && f.index == face.index).map(|f| f.id)
}

fn face_path(face: &fontdb::FaceInfo) -> Option<&Path> {
    match &face.source {
        fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => Some(path),
        fontdb::Source::Binary(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_info_strings() {
        assert!(is_mermaid(Some("mermaid")));
        assert!(is_mermaid(Some("Mermaid")));
        assert!(is_mermaid(Some("mermaid title=\"x\"")));
        assert!(!is_mermaid(Some("mermaidx")));
        assert!(!is_mermaid(Some("rust")));
        assert!(!is_mermaid(None));
    }
}
