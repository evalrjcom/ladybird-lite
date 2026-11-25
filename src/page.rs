//! DOM → flat "inline flow" of words, breaks and form controls (document order).
//!
//! This is a tiny stand-in for LibWeb's box-tree construction: block elements
//! become line breaks / paragraph gaps, inline text becomes words carrying a
//! style + optional link, and form elements become `Control` items that the
//! UI turns into real FLTK widgets.

use html5ever::{parse_document, tendril::TendrilSink, ParseOpts};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use url::Url;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextStyle {
    Normal,
    Bold,
    Heading(u8),
    Mono,
}

#[derive(Debug, Clone)]
pub enum Item {
    Word {
        text: String,
        style: TextStyle,
        link: Option<usize>,
        space_before: bool,
    },
    /// Force a new line (block boundary or `<br>`).
    Break,
    /// Paragraph gap (vertical whitespace).
    Gap,
    /// Horizontal rule (`<hr>`).
    Rule,
    /// An inline form control; index into `Page::controls`.
    Control { idx: usize, space_before: bool },
}

#[derive(Debug, Clone)]
pub enum ControlKind {
    Text { value: String, password: bool, size: Option<u32> },
    Hidden { value: String },
    Checkbox { value: String, checked: bool },
    Radio { value: String, checked: bool },
    /// Submit button. `value` is what gets submitted with `name` when it is the submitter.
    Submit { value: String, label: String },
    /// Non-submitting button (type=button/reset/file, …).
    Button { label: String },
    TextArea { value: String, rows: u32, cols: u32 },
    /// Options are (label, value).
    Select { options: Vec<(String, String)>, selected: usize },
}

#[derive(Debug, Clone)]
pub struct ControlSpec {
    pub form: Option<usize>,
    pub name: String,
    pub kind: ControlKind,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct FormSpec {
    pub action: Url,
    pub post: bool,
}

#[derive(Debug, Default)]
pub struct Page {
    pub title: Option<String>,
    pub items: Vec<Item>,
    pub links: Vec<Url>,
    pub forms: Vec<FormSpec>,
    pub controls: Vec<ControlSpec>,
    pub scripts: Vec<String>,
    pub styles: Vec<String>,
}

impl Page {
    /// A simple page showing a message (used for errors).
    pub fn message(title: &str, text: &str) -> Page {
        let mut b = Builder::new(Url::parse("about:blank").expect("static url"));
        b.words(title, TextStyle::Heading(2), None);
        b.gap();
        for line in text.lines() {
            b.words(line, TextStyle::Normal, None);
            b.items.push(Item::Break);
        }
        b.finish(Some(title.to_string()))
    }
}

pub fn parse(html: &str, url: &Url) -> anyhow::Result<Page> {
    let mut opts = ParseOpts::default();
    // We don't run JS against the DOM, so render <noscript> content like a no-JS browser.
    opts.tree_builder.scripting_enabled = false;
    let dom = parse_document(RcDom::default(), opts)
        .from_utf8()
        .read_from(&mut html.as_bytes())?;
    let mut b = Builder::new(url.clone());
    b.walk(&dom.document, Ctx::default());
    let title = b.title.take();
    Ok(b.finish(title))
}

#[derive(Clone, Copy)]
struct Ctx {
    style: TextStyle,
    link: Option<usize>,
    form: Option<usize>,
    pre: bool,
}

impl Default for Ctx {
    fn default() -> Self {
        Ctx { style: TextStyle::Normal, link: None, form: None, pre: false }
    }
}

struct Builder {
    base: Url,
    title: Option<String>,
    items: Vec<Item>,
    links: Vec<Url>,
    forms: Vec<FormSpec>,
    controls: Vec<ControlSpec>,
    scripts: Vec<String>,
    styles: Vec<String>,
    pending_space: bool,
}

fn local_name(h: &Handle) -> Option<String> {
    match &h.data {
        NodeData::Element { name, .. } => Some((*name.local).to_ascii_lowercase()),
        _ => None,
    }
}

fn attr(h: &Handle, key: &str) -> Option<String> {
    match &h.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .find(|a| (*a.name.local).eq_ignore_ascii_case(key))
            .map(|a| a.value.to_string()),
        _ => None,
    }
}

fn has_attr(h: &Handle, key: &str) -> bool {
    attr(h, key).is_some()
}

fn text_content(h: &Handle) -> String {
    let mut out = String::new();
    fn rec(h: &Handle, out: &mut String) {
        if let NodeData::Text { contents } = &h.data {
            out.push_str(&contents.borrow());
        }
        for c in h.children.borrow().iter() {
            rec(c, out);
        }
    }
    rec(h, &mut out);
    out
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

const BLOCKS: &[&str] = &[
    "address", "article", "aside", "blockquote", "center", "dd", "details", "dialog", "div", "dl",
    "dt", "fieldset", "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6",
    "header", "hr", "li", "main", "nav", "ol", "p", "pre", "section", "summary", "table", "tbody",
    "thead", "tfoot", "tr", "ul", "caption", "legend",
];
const GAPS: &[&str] = &[
    "p", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "dl", "blockquote", "pre", "table",
    "form", "fieldset", "figure",
];
const SKIP: &[&str] = &["template", "svg", "math", "iframe", "object", "canvas", "audio", "video", "map"];

impl Builder {
    fn new(base: Url) -> Self {
        Builder {
            base,
            title: None,
            items: Vec::new(),
            links: Vec::new(),
            forms: Vec::new(),
            controls: Vec::new(),
            scripts: Vec::new(),
            styles: Vec::new(),
            pending_space: false,
        }
    }

    fn finish(self, title: Option<String>) -> Page {
        Page {
            title,
            items: self.items,
            links: self.links,
            forms: self.forms,
            controls: self.controls,
            scripts: self.scripts,
            styles: self.styles,
        }
    }

    fn brk(&mut self) {
        self.pending_space = false;
        match self.items.last() {
            None | Some(Item::Break) | Some(Item::Gap) | Some(Item::Rule) => {}
            _ => self.items.push(Item::Break),
        }
    }

    fn gap(&mut self) {
        self.pending_space = false;
        match self.items.last() {
            None | Some(Item::Gap) | Some(Item::Rule) => {}
            Some(Item::Break) => {
                let n = self.items.len();
                self.items[n - 1] = Item::Gap;
            }
            _ => self.items.push(Item::Gap),
        }
    }

    /// Append whitespace-collapsed words.
    fn words(&mut self, s: &str, style: TextStyle, link: Option<usize>) {
        if s.starts_with(char::is_whitespace) {
            self.pending_space = true;
        }
        for w in s.split_whitespace() {
            self.items.push(Item::Word {
                text: w.to_string(),
                style,
                link,
                space_before: self.pending_space,
            });
            self.pending_space = true;
        }
        self.pending_space = s.ends_with(char::is_whitespace) || (s.trim().is_empty() && self.pending_space);
    }

    fn push_control(&mut self, spec: ControlSpec) {
        let hidden = matches!(spec.kind, ControlKind::Hidden { .. });
        self.controls.push(spec);
        if !hidden {
            let idx = self.controls.len() - 1;
            self.items.push(Item::Control { idx, space_before: self.pending_space });
            self.pending_space = true;
        }
    }

    fn resolve(&self, href: &str) -> Option<Url> {
        self.base.join(href.trim()).ok()
    }

    fn walk(&mut self, h: &Handle, ctx: Ctx) {
        match &h.data {
            NodeData::Document => {
                for c in h.children.borrow().iter() {
                    self.walk(c, ctx);
                }
            }
            NodeData::Text { contents } => {
                let s = contents.borrow().to_string();
                if ctx.pre {
                    let mut first = true;
                    for line in s.split('\n') {
                        if !first {
                            self.items.push(Item::Break);
                        }
                        first = false;
                        if !line.is_empty() {
                            self.items.push(Item::Word {
                                text: line.replace('\t', "    "),
                                style: TextStyle::Mono,
                                link: ctx.link,
                                space_before: false,
                            });
                        }
                    }
                } else {
                    self.words(&s, ctx.style, ctx.link);
                }
            }
            NodeData::Element { .. } => self.element(h, ctx),
            _ => {}
        }
    }

    fn children(&mut self, h: &Handle, ctx: Ctx) {
        for c in h.children.borrow().iter() {
            self.walk(c, ctx);
        }
    }

    fn element(&mut self, h: &Handle, mut ctx: Ctx) {
        let tag = local_name(h).unwrap_or_default();
        let tag = tag.as_str();
        match tag {
            "title" => {
                let t = collapse_ws(&text_content(h));
                if !t.is_empty() && self.title.is_none() {
                    self.title = Some(t);
                }
                return;
            }
            "script" => {
                let s = text_content(h);
                if !s.trim().is_empty() && attr(h, "src").is_none() {
                    self.scripts.push(s);
                }
                return;
            }
            "style" => {
                self.styles.push(text_content(h));
                return;
            }
            "base" => {
                if let Some(u) = attr(h, "href").and_then(|v| self.resolve(&v)) {
                    self.base = u;
                }
                return;
            }
            "br" => {
                self.items.push(Item::Break);
                self.pending_space = false;
                return;
            }
            "hr" => {
                self.brk();
                self.items.push(Item::Rule);
                return;
            }
            "img" => {
                if let Some(alt) = attr(h, "alt").map(|a| collapse_ws(&a)).filter(|a| !a.is_empty()) {
                    self.words(&format!(" [{}] ", alt), ctx.style, ctx.link);
                }
                return;
            }
            "input" => return self.input(h, ctx),
            "button" => {
                let kind = attr(h, "type").unwrap_or_else(|| "submit".into()).to_ascii_lowercase();
                let label = collapse_ws(&text_content(h));
                let label = if label.is_empty() { "Submit".to_string() } else { label };
                let k = if kind == "submit" {
                    ControlKind::Submit { value: attr(h, "value").unwrap_or_default(), label }
                } else {
                    ControlKind::Button { label }
                };
                self.push_control(ControlSpec {
                    form: ctx.form,
                    name: attr(h, "name").unwrap_or_default(),
                    kind: k,
                    disabled: has_attr(h, "disabled"),
                });
                return;
            }
            "textarea" => {
                let value = text_content(h);
                let value = value.strip_prefix('\n').unwrap_or(&value).to_string();
                let num = |k: &str, d: u32| attr(h, k).and_then(|v| v.trim().parse().ok()).unwrap_or(d);
                self.push_control(ControlSpec {
                    form: ctx.form,
                    name: attr(h, "name").unwrap_or_default(),
                    kind: ControlKind::TextArea { value, rows: num("rows", 3), cols: num("cols", 40) },
                    disabled: has_attr(h, "disabled"),
                });
                return;
            }
            "select" => {
                let mut options = Vec::new();
                let mut selected = None;
                fn collect(h: &Handle, opts: &mut Vec<(String, String)>, sel: &mut Option<usize>) {
                    for c in h.children.borrow().iter() {
                        match local_name(c).as_deref() {
                            Some("option") => {
                                let label = collapse_ws(&text_content(c));
                                let value = attr(c, "value").unwrap_or_else(|| label.clone());
                                if has_attr(c, "selected") {
                                    *sel = Some(opts.len());
                                }
                                opts.push((label, value));
                            }
                            Some("optgroup") => collect(c, opts, sel),
                            _ => {}
                        }
                    }
                }
                collect(h, &mut options, &mut selected);
                self.push_control(ControlSpec {
                    form: ctx.form,
                    name: attr(h, "name").unwrap_or_default(),
                    kind: ControlKind::Select { options, selected: selected.unwrap_or(0) },
                    disabled: has_attr(h, "disabled"),
                });
                return;
            }
            t if SKIP.contains(&t) => return,
            _ => {}
        }

        let is_block = BLOCKS.contains(&tag);
        let is_gap = GAPS.contains(&tag);
        if is_gap {
            self.gap();
        } else if is_block {
            self.brk();
        }

        match tag {
            "a" => {
                if let Some(u) = attr(h, "href")
                    .filter(|v| !v.trim_start().to_ascii_lowercase().starts_with("javascript:"))
                    .and_then(|v| self.resolve(&v))
                {
                    self.links.push(u);
                    ctx.link = Some(self.links.len() - 1);
                }
            }
            "b" | "strong" | "dt" => {
                if ctx.style == TextStyle::Normal {
                    ctx.style = TextStyle::Bold;
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                ctx.style = TextStyle::Heading(tag.as_bytes()[1] - b'0');
            }
            "code" | "tt" | "kbd" | "samp" => ctx.style = TextStyle::Mono,
            "pre" => {
                ctx.pre = true;
                ctx.style = TextStyle::Mono;
            }
            "li" => {
                self.items.push(Item::Word {
                    text: "•".into(),
                    style: TextStyle::Normal,
                    link: None,
                    space_before: false,
                });
                self.pending_space = true;
            }
            "td" => self.pending_space = true,
            "th" => {
                self.pending_space = true;
                if ctx.style == TextStyle::Normal {
                    ctx.style = TextStyle::Bold;
                }
            }
            "form" => {
                let action = attr(h, "action")
                    .filter(|a| !a.trim().is_empty())
                    .and_then(|a| self.resolve(&a))
                    .unwrap_or_else(|| self.base.clone());
                let post = attr(h, "method").is_some_and(|m| m.trim().eq_ignore_ascii_case("post"));
                self.forms.push(FormSpec { action, post });
                ctx.form = Some(self.forms.len() - 1);
            }
            _ => {}
        }

        self.children(h, ctx);

        if matches!(tag, "td" | "th") {
            self.pending_space = true;
        }
        if is_gap {
            self.gap();
        } else if is_block {
            self.brk();
        }
    }

    fn input(&mut self, h: &Handle, ctx: Ctx) {
        let ty = attr(h, "type").unwrap_or_else(|| "text".into()).trim().to_ascii_lowercase();
        let value = attr(h, "value");
        let checked = has_attr(h, "checked");
        let kind = match ty.as_str() {
            "hidden" => ControlKind::Hidden { value: value.unwrap_or_default() },
            "checkbox" => ControlKind::Checkbox { value: value.unwrap_or_else(|| "on".into()), checked },
            "radio" => ControlKind::Radio { value: value.unwrap_or_else(|| "on".into()), checked },
            "submit" => {
                let label = value.clone().filter(|v| !v.is_empty()).unwrap_or_else(|| "Submit".into());
                ControlKind::Submit { value: value.unwrap_or_else(|| label.clone()), label }
            }
            "image" => ControlKind::Submit {
                value: String::new(),
                label: attr(h, "alt").filter(|a| !a.is_empty()).unwrap_or_else(|| "Submit".into()),
            },
            "button" | "reset" => ControlKind::Button {
                label: value.unwrap_or_else(|| if ty == "reset" { "Reset".into() } else { String::new() }),
            },
            "file" => ControlKind::Button { label: "(file upload unsupported)".into() },
            _ => ControlKind::Text {
                value: value.unwrap_or_default(),
                password: ty == "password",
                size: attr(h, "size").and_then(|s| s.trim().parse().ok()),
            },
        };
        self.push_control(ControlSpec {
            form: ctx.form,
            name: attr(h, "name").unwrap_or_default(),
            kind,
            disabled: has_attr(h, "disabled"),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ddg_lite_like_form() {
        let html = r#"<html><head><title>DuckDuckGo</title></head><body>
            <form action="/lite/" method="post">
              <input class="query" type="text" size="40" name="q" value="">
              <input class="submit" type="submit" value="Search">
              <select name="kl"><option value="">All Regions</option><option value="us-en" selected>US</option></select>
              <input type="hidden" name="df" value="">
            </form>
            <p>Hello <a href="/about">about <b>us</b></a></p></body></html>"#;
        let url = Url::parse("https://lite.duckduckgo.com/lite/").unwrap();
        let p = parse(html, &url).unwrap();
        assert_eq!(p.title.as_deref(), Some("DuckDuckGo"));
        assert_eq!(p.forms.len(), 1);
        assert_eq!(p.forms[0].action.as_str(), "https://lite.duckduckgo.com/lite/");
        assert!(p.forms[0].post);
        assert_eq!(p.controls.len(), 4);
        assert!(matches!(p.controls[2].kind, ControlKind::Select { selected: 1, .. }));
        assert_eq!(p.links[0].as_str(), "https://lite.duckduckgo.com/about");
        let linked: Vec<_> = p
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Word { text, link: Some(_), .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(linked, ["about", "us"]);
    }

    #[test]
    fn whitespace_between_inline_elements() {
        let url = Url::parse("https://e.x/").unwrap();
        let p = parse("<p>foo<b>bar</b> baz</p>", &url).unwrap();
        let w: Vec<_> = p
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Word { text, space_before, .. } => Some((text.as_str(), *space_before)),
                _ => None,
            })
            .collect();
        assert_eq!(w, [("foo", false), ("bar", false), ("baz", true)]);
    }
}
