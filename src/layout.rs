//! Inline flow layout + painting (tiny stand-in for LibWeb layout/paint).
//!
//! Words and form controls are placed left-to-right and wrapped at the
//! available width; there is never horizontal overflow (over-long words are
//! broken by characters). Re-run on every resize.

use crate::page::{ControlKind, ControlSpec, Item, TextStyle};
use fltk::{draw, enums::Font};

pub const MARGIN: i32 = 10;
const LINE_GAP: i32 = 3;
const PARA_GAP: i32 = 10;
const BASE_SIZE: i32 = 15;

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
    pub words: Vec<PlacedWord>,
    pub controls: Vec<PlacedControl>,
    pub rules: Vec<i32>,
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

/// Preferred size of a control's widget.
pub fn control_size(spec: &ControlSpec) -> (i32, i32) {
    draw::set_font(Font::Helvetica, 14);
    let text_w = |s: &str| draw::width(s) as i32;
    match &spec.kind {
        ControlKind::Text { size, .. } => {
            let chars = size.unwrap_or(30).clamp(8, 60) as i32;
            (chars * 8 + 10, 28)
        }
        ControlKind::Hidden { .. } => (0, 0),
        ControlKind::Checkbox { .. } | ControlKind::Radio { .. } => (24, 24),
        ControlKind::Submit { label, .. } | ControlKind::Button { label } => {
            (text_w(label).max(40) + 28, 28)
        }
        ControlKind::TextArea { rows, cols, .. } => {
            ((*cols).clamp(10, 100) as i32 * 8 + 10, (*rows).clamp(2, 20) as i32 * 18 + 8)
        }
        ControlKind::Select { options, .. } => {
            let w = options.iter().map(|(l, _)| text_w(l)).max().unwrap_or(40);
            (w.clamp(40, 360) + 40, 28)
        }
    }
}

enum Entry {
    Word(PlacedWord),
    Ctrl(PlacedControl),
}

struct Flow {
    width: i32,
    x: i32,
    y: i32,
    line: Vec<Entry>,
    out: Layout,
}

impl Flow {
    fn finish_line(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let mut line_h = 0;
        for e in &self.line {
            line_h = line_h.max(match e {
                Entry::Word(w) => w.h,
                Entry::Ctrl(c) => c.h,
            });
        }
        for e in self.line.drain(..) {
            match e {
                Entry::Word(mut w) => {
                    let (f, s) = font_for(w.style);
                    draw::set_font(f, s);
                    // bottom-align text to a common baseline area, centred vs. tall controls
                    w.y = self.y + (line_h - w.h) / 2;
                    w.baseline = w.y + w.h - draw::descent();
                    self.out.words.push(w);
                }
                Entry::Ctrl(mut c) => {
                    c.y = self.y + (line_h - c.h) / 2;
                    self.out.controls.push(c);
                }
            }
        }
        self.y += line_h + LINE_GAP;
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
}

pub fn layout(items: &[Item], controls: &[ControlSpec], total_width: i32) -> Layout {
    let width = (total_width - 2 * MARGIN).max(60);
    let mut f = Flow { width, x: MARGIN, y: MARGIN, line: Vec::new(), out: Layout::default() };
    draw::set_font(Font::Helvetica, BASE_SIZE);
    let empty_line_h = draw::height();

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
            Item::Word { text, style, link, space_before } => {
                let (font, size) = font_for(*style);
                draw::set_font(font, size);
                let h = draw::height();
                let space = if *space_before { draw::width(" ") as i32 } else { 0 };
                // Break over-long words into chunks that fit the line.
                let mut rest: &str = text;
                let mut sp = space;
                while !rest.is_empty() {
                    let full = draw::width(rest) as i32;
                    let (chunk, tail) = if full <= width {
                        (rest, "")
                    } else {
                        let avail = if f.line.is_empty() { width } else { MARGIN + width - f.x - sp };
                        let mut end = 0;
                        for (i, ch) in rest.char_indices() {
                            let e = i + ch.len_utf8();
                            if draw::width(&rest[..e]) as i32 > avail {
                                break;
                            }
                            end = e;
                        }
                        if end == 0 {
                            if f.line.is_empty() {
                                end = rest.chars().next().map_or(rest.len(), char::len_utf8);
                            } else {
                                f.finish_line();
                                sp = 0;
                                continue;
                            }
                        }
                        (&rest[..end], &rest[end..])
                    };
                    let w = draw::width(chunk) as i32;
                    let x = f.place(w, sp);
                    f.line.push(Entry::Word(PlacedWord {
                        x,
                        y: 0,
                        w,
                        h,
                        baseline: 0,
                        text: chunk.to_string(),
                        style: *style,
                        link: *link,
                    }));
                    rest = tail;
                    sp = 0;
                    if !rest.is_empty() {
                        f.finish_line();
                    }
                }
            }
            Item::Control { idx, space_before } => {
                let (w, h) = control_size(&controls[*idx]);
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

/// Paint the text layer at origin (ox, oy).
pub fn paint(l: &Layout, ox: i32, oy: i32, w: i32, h: i32) {
    draw::draw_rect_fill(ox, oy, w, h, BG);
    let mut prev: Option<&PlacedWord> = None;
    for word in &l.words {
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
    for &ry in &l.rules {
        draw::draw_line(ox + MARGIN, oy + ry, ox + w - MARGIN, oy + ry);
    }
}

/// Which link (index into `Page::links`) is under the point (relative coords)?
pub fn link_at(l: &Layout, x: i32, y: i32) -> Option<usize> {
    l.words.iter().find_map(|w| {
        (w.link.is_some() && x >= w.x && x < w.x + w.w && y >= w.y && y < w.y + w.h).then_some(w.link)?
    })
}
