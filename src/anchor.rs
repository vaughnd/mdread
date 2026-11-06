//! Keeping the reader's place while the content above it changes height, and easing key scrolls.
//!
//! A place is a block and how far the view top is into it. After each pass the app works out
//! where that place now is; if it is not where the pass drew it (an image or diagram above
//! arrived, a block above was measured for the first time, the column was rewrapped), it
//! corrects the scroll offset and discards the pass, so the frame shown is drawn from the
//! corrected offset. Everything here goes through egui's public API: the scroll offset is set
//! with `ScrollArea::vertical_scroll_offset` and the pass discarded with
//! `Context::request_discard`.
//!
//! Key scrolls (PageDown, Space, `j`, `k`, arrows) are eased here rather than by egui, because
//! egui keeps its own target for them that a correction cannot move: the ease would drag the
//! view back by whatever grew above it.

use egui::emath::{ease_in_ease_out, interpolation_factor, lerp};
use egui::style::ScrollAnimation;

/// The reader's place in the document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Place {
    pub block: usize,
    /// How far the view top is below the block's top. Negative when the block starts below the
    /// view top, as a jump leaves its target a gap below it.
    pub into: f32,
    /// The block's height when the place was taken, or 0 when it was still loading or not yet
    /// measured. A place inside a block that is later laid out at another height (a width or
    /// zoom change) is kept in proportion; a loading block's height says nothing.
    pub h: f32,
    /// Kept until the reader scrolls: a jump, or a place restored by Back, Forward or reload.
    /// Any other place is taken afresh from the view at the end of every pass.
    pub held: bool,
}

/// The block containing content offset `y`, and how far `y` is below its top. `tops` are the
/// block tops in the same coordinates, in order. `None` above the first block, in the padding
/// at the page top, where nothing above can move the view.
pub fn place_at(tops: &[f32], y: f32) -> Option<(usize, f32)> {
    let i = tops.partition_point(|t| *t <= y);
    (i > 0).then(|| (i - 1, y - tops[i - 1]))
}

/// The content offset that puts the view top at place `p`, given where the block's top is now,
/// its height now and whether it is still loading. `line` is about a line of text.
///
/// A place inside a block whose settled height differs from the one it was taken with is kept
/// in proportion: 800 pt into a diagram that halves becomes 400 pt into it. A place deeper than
/// a block that is still loading (Back to a diagram not laid out yet, or a diagram redrawn for a
/// new theme) is drawn a line inside the block, so the view stays on it, and comes back when the
/// block regains its height.
pub fn offset_for(p: Place, top: f32, h: f32, loading: bool, line: f32) -> f32 {
    let mut into = p.into;
    if into > 0.0 && p.h > 0.0 && !loading && h > 0.0 && (h - p.h).abs() >= 0.5 {
        into *= h / p.h;
    }
    if loading && into > h {
        into = (h - line).max(0.0);
    }
    top + into
}

/// Whether place `p` is waiting for its block to load: it is deeper than the block is now. Such
/// a place is kept rather than taken afresh from the view, which would land on a later block.
pub fn waiting(p: Place, h: f32, loading: bool) -> bool {
    loading && p.into > h
}

/// A key scroll eased towards `target`, as egui eases its own: ease in and out over a time
/// span that grows with the distance, from 0.1 to 0.3 s by default. More presses while it runs
/// move the target and keep the span, so a held key does not restart the ease.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyScroll {
    pub target: f32,
    span: (f64, f64),
    /// The time of the last step, so a pass egui runs again in the same frame does not step twice.
    last: f64,
}

impl KeyScroll {
    /// Scrolls by `delta` from offset `from`, or moves the target of the scroll `running` by it.
    /// The target stays within `0..=max`.
    pub fn add(running: Option<KeyScroll>, from: f32, delta: f32, max: f32, now: f64, anim: ScrollAnimation) -> KeyScroll {
        match running {
            Some(k) => KeyScroll { target: (k.target + delta).clamp(0.0, max), ..k },
            None => {
                let target = (from + delta).clamp(0.0, max);
                let secs = ((target - from).abs() / anim.points_per_second).clamp(anim.duration.min, anim.duration.max);
                KeyScroll { target, span: (now, now + secs as f64), last: now }
            }
        }
    }

    /// The offset for time `now`, coming from `from`, and whether the scroll has arrived. `None`
    /// when `now` was stepped already.
    pub fn step(&mut self, from: f32, now: f64) -> Option<(f32, bool)> {
        if now <= self.last {
            return None;
        }
        let dt = (now - self.last) as f32;
        self.last = now;
        if (from - self.target).abs() < 1.0 {
            return Some((self.target, true));
        }
        let t = interpolation_factor(self.span, now, dt, ease_in_ease_out);
        Some(if t < 1.0 { (lerp(from..=self.target, t), false) } else { (self.target, true) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(block: usize, into: f32, h: f32) -> Place {
        Place { block, into, h, held: false }
    }

    #[test]
    fn place_at_names_the_block_containing_y() {
        let tops = [20.0, 100.0, 300.0];
        assert_eq!(place_at(&tops, 10.0), None);
        assert_eq!(place_at(&tops, 20.0), Some((0, 0.0)));
        assert_eq!(place_at(&tops, 150.0), Some((1, 50.0)));
        assert_eq!(place_at(&tops, 300.0), Some((2, 0.0)));
        assert_eq!(place_at(&tops, 5000.0), Some((2, 4700.0)));
        assert_eq!(place_at(&[], 5.0), None);
    }

    #[test]
    fn offset_for_keeps_pixels_scales_settled_blocks_and_waits_for_loading_ones() {
        // Growth above only moves the top.
        assert_eq!(offset_for(place(3, 40.0, 200.0), 1000.0, 200.0, false, 25.0), 1040.0);
        // A jump's target a gap below the view top.
        assert_eq!(offset_for(place(3, -15.0, 0.0), 1000.0, 200.0, false, 25.0), 985.0);
        // Rewrapped to half the height: half as far in.
        assert_eq!(offset_for(place(3, 800.0, 1200.0), 1000.0, 600.0, false, 25.0), 1400.0);
        // Taken while loading: no height to scale from.
        assert_eq!(offset_for(place(3, 10.0, 0.0), 1000.0, 1500.0, false, 25.0), 1010.0);
        // Deeper than a loading placeholder: a line inside it, and back once it is laid out.
        assert_eq!(offset_for(place(3, 800.0, 1500.0), 1000.0, 40.0, true, 25.0), 1015.0);
        assert_eq!(offset_for(place(3, 800.0, 1500.0), 1000.0, 1500.0, false, 25.0), 1800.0);
        // Below the last block (the bottom padding) of a settled page: left alone.
        assert_eq!(offset_for(place(3, 300.0, 200.0), 1000.0, 200.0, false, 25.0), 1300.0);
        assert!(waiting(place(3, 800.0, 1500.0), 40.0, true));
        assert!(!waiting(place(3, 30.0, 1500.0), 40.0, true));
        assert!(!waiting(place(3, 800.0, 1500.0), 600.0, false));
    }

    #[test]
    fn a_key_scroll_eases_in_and_out_and_retargets_without_restarting() {
        let anim = ScrollAnimation::default();
        let mut k = KeyScroll::add(None, 100.0, 300.0, 10_000.0, 1.0, anim);
        assert_eq!(k.target, 400.0);
        // A 300 pt scroll at 1000 pt/s takes 0.3 s.
        assert!((k.span.1 - 1.3).abs() < 1e-6);
        assert_eq!(k.step(100.0, 1.0), None, "the same time steps once");
        let mut off = 100.0;
        let mut steps = Vec::new();
        let mut now = 1.0;
        loop {
            now += 1.0 / 60.0;
            let (o, done) = k.step(off, now).unwrap();
            steps.push(o - off);
            off = o;
            if done {
                break;
            }
        }
        assert_eq!(off, 400.0);
        assert!(steps.len() >= 17 && steps.len() <= 20, "{} steps", steps.len());
        // Slow, faster, slow.
        let mid = steps[steps.len() / 2];
        assert!(steps[0] < mid && *steps.last().unwrap() < mid, "{steps:?}");

        // A press while it runs moves the target, not the span; the target stays in range.
        let k = KeyScroll::add(None, 0.0, 300.0, 10_000.0, 0.0, anim);
        let k2 = KeyScroll::add(Some(k), 50.0, 300.0, 500.0, 0.1, anim);
        assert_eq!(k2.target, 500.0);
        assert_eq!(k2.span, k.span);
        assert_eq!(KeyScroll::add(None, 20.0, -300.0, 500.0, 0.0, anim).target, 0.0);
    }
}
