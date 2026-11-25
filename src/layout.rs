//! Inline flow layout + painting (tiny stand-in for `LibWeb` layout/paint).
//!
//! Words and form controls are placed left-to-right and wrapped at the
//! available width; there is never horizontal overflow (over-long words are
//! broken by characters). Re-run on every resize.

use crate::page::{ControlKind, ControlSpec, Item, TextStyle};
use fltk::{draw, enums::Font};
use std::collections::HashMap;

pub const MARGIN: i32 = 10;
const LINE_GAP: i32 = 3;
const PARA_GAP: i32 = 10;
const BASE_SIZE: i32 = 15;
/// Smallest content width we lay out for (tiny/collapsed windows).
const MIN_WIDTH: i32 = 60;

#[derive(Debug, Clone)]
pub struct PlacedWord {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub baseline: i32,
    pub text: String,
    pub style: TextStyle,
    pub link: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct PlacedControl {
    pub idx: usize,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Default)]
pub struct Layout {
    /// Sorted by `y` (document order), which `paint`/`link_at` rely on for binary search.
    pub words: Vec<PlacedWord>,
    pub controls: Vec<PlacedControl>,
    pub rules: Vec<i32>,
    // AUDIT FIX: fragment targets (#id) → y offset, so in-page links can scroll.
    pub anchors: HashMap<String, i32>,
    pub height: i32,
    /// Viewport (scroll area) size this layout was computed for.
    pub viewport: (i32, i32),
}

pub fn font_for(style: TextStyle) -> (Font, i32) {
    match style {
        TextStyle::Normal => (Font::Helvetica, BASE_SIZE),
        TextStyle::Bold => (Font::HelveticaBold, BASE_SIZE),
        TextStyle::Mono => (Font::Courier, BASE_SIZE - 1),
        TextStyle::Heading(1) => (Font::HelveticaBold, 26),
        TextStyle::Heading(2) => (Font::HelveticaBold, 22),
        TextStyle::Heading(3) => (Font::HelveticaBold, 19),
        TextStyle::Heading(_) => (Font::HelveticaBold, 17),
    }
}

/// Text measurement backend. FLTK in the app; a fixed-width fake in unit tests.
// AUDIT FIX: layout was hard-wired to fltk::draw (needs a display), so wrapping/UTF-8 breaking
// had no tests; abstracting measurement made the regressions below testable.
pub trait Measure {
    fn set_style(&mut self, style: TextStyle);
    fn width(&mut self, s: &str) -> i32;
    fn line_height(&mut self) -> i32;
    fn descent(&mut self) -> i32;
}

/// Measures with the current FLTK display fonts.
pub struct FltkMeasure;

impl Measure for FltkMeasure {
    fn set_style(&mut self, style: TextStyle) {
        let (f, s) = font_for(style);
        draw::set_font(f, s);
    }
    #[allow(clippy::cast_possible_truncation)] // pixel widths are far below i32::MAX
    fn width(&mut self, s: &str) -> i32 {
        draw::width(s).ceil() as i32
    }
    fn line_height(&mut self) -> i32 {
        draw::height()
    }
    fn descent(&mut self) -> i32 {
        draw::descent()
    }
}

/// Preferred size of a control's widget.
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)] // all values clamped first
pub fn control_size(spec: &ControlSpec, m: &mut dyn Measure) -> (i32, i32) {
    m.set_style(TextStyle::Normal);
    match &spec.kind {
        ControlKind::Text { size, .. } => {
            let chars = size.unwrap_or(30).clamp(8, 60) as i32;
            (chars * 8 + 10, 28)
        }
        ControlKind::Hidden { .. } => (0, 0),
        ControlKind::Checkbox { .. } | ControlKind::Radio { .. } => (24, 24),
        ControlKind::Submit { label, .. } | ControlKind::Button { label } => (m.width(label).max(40) + 28, 28),
        ControlKind::TextArea { rows, cols, .. } => {
            // AUDIT FIX: `*cols as i32` could wrap for huge attribute values; clamp in u32 first.
            ((*cols).clamp(10, 100) as i32 * 8 + 10, (*rows).clamp(2, 20) as i32 * 18 + 8)
        }
        ControlKind::Select { options, multiple, size, .. } => {
            let w = options.iter().take(500).map(|o| m.width(&o.label)).max().unwrap_or(40);
            let w = w.clamp(40, 360) + 40;
            if *multiple || size.is_some_and(|s| s > 1) {
                // Rendered as a list box.
                let rows = size.unwrap_or(4).clamp(2, 10) as i32;
                (w, rows * 20 + 4)
            } else {
                (w, 28)
            }
        }
    }
}

enum Entry {
    Word(PlacedWord),
    Ctrl(PlacedControl),
}

struct Flow<'m> {
    m: &'m mut dyn Measure,
    width: i32,
    x: i32,
    y: i32,
    line: Vec<Entry>,
    out: Layout,
}

impl Flow<'_> {
    fn finish_line(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let line_h = self
            .line
            .iter()
            .map(|e| match e {
                Entry::Word(w) => w.h,
                Entry::Ctrl(c) => c.h,
            })
            .max()
            .unwrap_or(0);
        for e in std::mem::take(&mut self.line) {
            match e {
                Entry::Word(mut w) => {
                    self.m.set_style(w.style);
                    // centre text vertically against taller controls on the same line
                    w.y = self.y + (line_h - w.h) / 2;
                    w.baseline = w.y + w.h - self.m.descent();
                    self.out.words.push(w);
                }
                Entry::Ctrl(mut c) => {
                    c.y = self.y + (line_h - c.h) / 2;
                    self.out.controls.push(c);
                }
            }
        }
        self.y = self.y.saturating_add(line_h + LINE_GAP);
        self.x = MARGIN;
    }

    /// Place a box of width `w`; wraps first if it doesn't fit. Returns its x.
    fn place(&mut self, w: i32, space: i32) -> i32 {
        let space = if self.line.is_empty() { 0 } else { space };
        if !self.line.is_empty() && self.x + space + w > MARGIN + self.width {
            self.finish_line();
            return self.place(w, 0);
        }
        let x = self.x + space;
        self.x = x + w;
        x
    }

    /// Largest prefix of `s` (ending on a char boundary) whose width fits `avail`, at least one char.
    // AUDIT FIX: the old loop re-measured `&rest[..e]` for every char (O(n²)): a single 1 MB
    // token (base64 blob, minified JS shown as text) froze the UI for minutes. Now O(n) by
    // summing per-char widths; always slices on `char_indices` boundaries (UTF-8 safe).
    fn fitting_prefix<'s>(&mut self, s: &'s str, avail: i32) -> (&'s str, &'s str) {
        let mut used = 0;
        let mut end = 0;
        let mut buf = [0u8; 4];
        for (i, ch) in s.char_indices() {
            used += self.m.width(ch.encode_utf8(&mut buf));
            if used > avail && end > 0 {
                break;
            }
            end = i + ch.len_utf8();
            if used > avail {
                break; // first char alone is wider than the line: take it anyway
            }
        }
        s.split_at(end)
    }

    fn push_word(&mut self, text: &str, w: i32, h: i32, sp: i32, style: TextStyle, link: Option<usize>) {
        let x = self.place(w, sp);
        self.line.push(Entry::Word(PlacedWord { x, y: 0, w, h, baseline: 0, text: text.to_string(), style, link }));
    }

    fn word(&mut self, text: &str, style: TextStyle, link: Option<usize>, space_before: bool) {
        /// Words up to this many bytes are measured whole (accurate kerning); longer ones are
        /// necessarily split and measured per char (linear time).
        const WHOLE_MEASURE_BYTES: usize = 256;
        self.m.set_style(style);
        let h = self.m.line_height();
        let mut sp = if space_before { self.m.width(" ") } else { 0 };
        let mut rest = text;
        loop {
            if rest.len() <= WHOLE_MEASURE_BYTES {
                let full = self.m.width(rest);
                if full <= self.width {
                    self.push_word(rest, full, h, sp, style, link); // place() wraps if needed
                    return;
                }
            }
            // Wider than a whole line: fill the current line's remainder, continue below.
            let avail = if self.line.is_empty() { self.width } else { MARGIN + self.width - self.x - sp };
            if !self.line.is_empty() && avail < self.m.width("W") {
                self.finish_line();
                sp = 0;
                continue;
            }
            let (chunk, tail) = self.fitting_prefix(rest, avail.max(1));
            let w = self.m.width(chunk);
            self.push_word(chunk, w, h, sp, style, link);
            if tail.is_empty() {
                return;
            }
            sp = 0;
            rest = tail;
            self.finish_line();
        }
    }
}

#[allow(clippy::many_single_char_names)] // x/y/w/h/m are the conventional geometry names
pub fn layout(items: &[Item], controls: &[ControlSpec], total_width: i32, m: &mut dyn Measure) -> Layout {
    let width = (total_width - 2 * MARGIN).max(MIN_WIDTH);
    m.set_style(TextStyle::Normal);
    let empty_line_h = m.line_height();
    let mut f = Flow { m, width, x: MARGIN, y: MARGIN, line: Vec::new(), out: Layout::default() };

    let mut prev_break = false;
    for item in items {
        match item {
            Item::Break => {
                if f.line.is_empty() && prev_break {
                    f.y += empty_line_h; // consecutive <br>: blank line
                }
                f.finish_line();
                prev_break = true;
                continue;
            }
            Item::Gap => {
                f.finish_line();
                f.y += PARA_GAP;
            }
            Item::Rule => {
                f.finish_line();
                f.out.rules.push(f.y + 4);
                f.y += 10;
            }
            Item::Anchor(id) => {
                // Top of the current line (the anchor's content starts here or on the next line).
                f.out.anchors.entry(id.clone()).or_insert(f.y);
                continue; // doesn't affect <br> handling
            }
            Item::Word { text, style, link, space_before } => f.word(text, *style, *link, *space_before),
            Item::Control { idx, space_before } => {
                // AUDIT FIX: out-of-range control index (malformed Page) is skipped, not a panic.
                let Some(spec) = controls.get(*idx) else { continue };
                let (w, h) = control_size(spec, f.m);
                let w = w.min(width);
                let space = if *space_before { 6 } else { 0 };
                let x = f.place(w, space);
                f.line.push(Entry::Ctrl(PlacedControl { idx: *idx, x, y: 0, w, h }));
            }
        }
        prev_break = false;
    }
    f.finish_line();
    f.out.height = f.y + MARGIN;
    f.out
}

pub const BG: fltk::enums::Color = fltk::enums::Color::from_rgb(25, 25, 25);
const FG: fltk::enums::Color = fltk::enums::Color::from_rgb(225, 225, 225);
const LINK: fltk::enums::Color = fltk::enums::Color::from_rgb(120, 175, 255);

/// Index range of words that intersect rows `[top, bottom)` (words are sorted by `y`).
fn visible_words(l: &Layout, top: i32, bottom: i32) -> std::ops::Range<usize> {
    // Tallest word is a heading (< 64px); search from a margin above `top`.
    let start = l.words.partition_point(|w| w.y + 64 < top);
    let end = l.words.partition_point(|w| w.y < bottom);
    start..end.max(start)
}

/// Paint the text layer at origin (ox, oy); only rows `[vis_top, vis_bottom)` (layout coords).
// AUDIT FIX: every redraw iterated all words (hundreds of thousands on big pages); now only the
// visible slice found by binary search is drawn.
#[allow(clippy::many_single_char_names, clippy::too_many_arguments)]
pub fn paint(l: &Layout, ox: i32, oy: i32, w: i32, h: i32, vis_top: i32, vis_bottom: i32) {
    draw::draw_rect_fill(ox, oy, w, h, BG);
    let range = visible_words(l, vis_top, vis_bottom);
    let mut prev: Option<&PlacedWord> = range.start.checked_sub(1).and_then(|i| l.words.get(i));
    for word in &l.words[range] {
        let (x, y) = (ox + word.x, oy + word.y);
        let prev_word = prev.replace(word);
        if !draw::not_clipped(x, y, word.w, word.h) {
            continue;
        }
        let (font, size) = font_for(word.style);
        draw::set_font(font, size);
        draw::set_draw_color(if word.link.is_some() { LINK } else { FG });
        draw::draw_text(&word.text, x, oy + word.baseline);
        if word.link.is_some() {
            let uy = oy + word.baseline + 2;
            // Underline the gap too when the previous word on this line is the same link.
            let x0 = match prev_word {
                Some(p) if p.link == word.link && p.baseline == word.baseline => ox + p.x + p.w,
                _ => x,
            };
            draw::draw_line(x0, uy, x + word.w, uy);
        }
    }
    draw::set_draw_color(fltk::enums::Color::from_rgb(90, 90, 90));
    for &ry in l.rules.iter().filter(|&&ry| ry >= vis_top - 2 && ry <= vis_bottom + 2) {
        draw::draw_line(ox + MARGIN, oy + ry, ox + w - MARGIN, oy + ry);
    }
}

/// Which link (index into `Page::links`) is under the point (relative coords)?
pub fn link_at(l: &Layout, x: i32, y: i32) -> Option<usize> {
    l.words[visible_words(l, y, y + 1)].iter().find_map(|w| {
        (w.link.is_some() && x >= w.x && x < w.x + w.w && y >= w.y && y < w.y + w.h).then_some(w.link)?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every char is 10px wide, lines are 20px.
    struct Fixed;
    impl Measure for Fixed {
        fn set_style(&mut self, _: TextStyle) {}
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        fn width(&mut self, s: &str) -> i32 {
            s.chars().count() as i32 * 10
        }
        fn line_height(&mut self) -> i32 {
            20
        }
        fn descent(&mut self) -> i32 {
            4
        }
    }

    fn word(t: &str, space: bool) -> Item {
        Item::Word { text: t.into(), style: TextStyle::Normal, link: None, space_before: space }
    }

    fn lines(l: &Layout) -> Vec<String> {
        let mut out: Vec<(i32, String)> = Vec::new();
        for w in &l.words {
            match out.last_mut() {
                Some((y, s)) if *y == w.y => {
                    s.push('|');
                    s.push_str(&w.text);
                }
                _ => out.push((w.y, w.text.clone())),
            }
        }
        out.into_iter().map(|(_, s)| s).collect()
    }

    #[test]
    fn wraps_at_width_without_overflow() {
        // width 100 → content width 80 → 8 chars per line
        let items = [word("aaa", false), word("bbb", true), word("ccc", true)];
        let l = layout(&items, &[], 100, &mut Fixed);
        assert_eq!(lines(&l), ["aaa|bbb", "ccc"]);
        assert!(l.words.iter().all(|w| w.x + w.w <= 100 - MARGIN));
    }

    #[test]
    fn long_multibyte_word_breaks_on_char_boundaries() {
        // 30 × 'é' (2 bytes each) + emoji (4 bytes) at width 100 → 8 chars per line, no panic
        let long = format!("{}😀", "é".repeat(30));
        let l = layout(&[word(&long, false)], &[], 100, &mut Fixed);
        let got = lines(&l);
        assert!(got.iter().all(|s| s.chars().count() <= 8), "{got:?}");
        assert_eq!(got.concat(), long);
    }

    #[test]
    fn huge_token_is_linear_time() {
        let long = "A".repeat(1_000_000);
        let t = std::time::Instant::now();
        let l = layout(&[word(&long, false)], &[], 1000, &mut Fixed);
        assert_eq!(l.words.len(), 1_000_000 / 98 + 1);
        assert!(t.elapsed().as_secs() < 5);
    }

    #[test]
    fn tiny_width_and_anchor_positions() {
        let items = [word("x", false), Item::Break, Item::Anchor("a".into()), word("yyyyyyyyyy", false)];
        let l = layout(&items, &[], -50, &mut Fixed); // collapsed window
        assert!(l.height > 0);
        assert_eq!(l.anchors.get("a"), Some(&(MARGIN + 20 + LINE_GAP)));
    }

    #[test]
    fn visible_range_and_hit_test() {
        let mut items = Vec::new();
        for i in 0..1000 {
            items.push(Item::Word { text: format!("w{i}"), style: TextStyle::Normal, link: Some(i), space_before: false });
            items.push(Item::Break);
        }
        let l = layout(&items, &[], 400, &mut Fixed);
        let r = visible_words(&l, 2300, 2400);
        assert!(r.len() < 10 && !r.is_empty());
        let w = &l.words[500];
        assert_eq!(link_at(&l, w.x + 1, w.y + 1), Some(500));
        assert_eq!(link_at(&l, w.x + 1, -100), None);
    }
}
