//! DOM → flat "inline flow" of words, breaks and form controls (document order).
//!
//! This is a tiny stand-in for `LibWeb`'s box-tree construction: block elements
//! become line breaks / paragraph gaps, inline text becomes words carrying a
//! style + optional link, and form elements become `Control` items that the
//! UI turns into real FLTK widgets.

use html5ever::{parse_document, tendril::TendrilSink, ParseOpts};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use std::collections::HashMap;
use url::Url;

// AUDIT FIX: hostile/huge pages. Deeply nested markup (e.g. 50k nested <b><i>) overflowed the
// stack in the recursive walk (crash), and multi-megabyte pages produced millions of items.
/// Elements deeper than this are flattened to plain text (iteratively, no recursion).
const MAX_DEPTH: usize = 256;
/// Hard cap on flow items; the rest of the page is replaced by a notice.
pub const MAX_ITEMS: usize = 400_000;

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
    // AUDIT FIX: fragment links (#id) had no target; element ids / <a name> are now recorded.
    /// Fragment target (`id=` / `<a name=>`).
    Anchor(String),
}

/// Per-submitter overrides (`formaction`, `formmethod`, `formenctype`).
// AUDIT FIX: these attributes on submit buttons were ignored, so e.g. a "Preview" button with
// formaction=/preview posted to the form's default action.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubmitOverrides {
    pub action: Option<Url>,
    pub post: Option<bool>,
    pub multipart: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub label: String,
    pub value: String,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub enum ControlKind {
    // AUDIT FIX: + maxlength (was ignored).
    Text { value: String, password: bool, size: Option<u32>, maxlength: Option<usize> },
    Hidden { value: String },
    Checkbox { value: String, checked: bool },
    Radio { value: String, checked: bool },
    /// Submit button. `value` is what gets submitted with `name` when it is the submitter.
    // AUDIT FIX: `image` submits send name.x/name.y (not name=value) per the HTML spec.
    Submit { value: String, label: String, image: bool, overrides: SubmitOverrides },
    /// Non-submitting button (type=button/reset/file, …).
    Button { label: String },
    TextArea { value: String, rows: u32, cols: u32 },
    // AUDIT FIX: <select multiple> submitted only one value and disabled options could be the
    // default; now a list of selected indices + per-option disabled flag.
    Select { options: Vec<SelectOption>, selected: Vec<usize>, multiple: bool, size: Option<u32> },
}

#[derive(Debug, Clone)]
pub struct ControlSpec {
    pub form: Option<usize>,
    pub name: String,
    pub kind: ControlKind,
    pub disabled: bool,
    // AUDIT FIX: readonly fields were editable; autofocus is now honoured instead of always
    // focusing the first text field (which made Space/arrows type into it instead of scrolling).
    pub readonly: bool,
    pub autofocus: bool,
    /// `form="id"` owner attribute (resolved after parsing).
    form_attr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FormSpec {
    pub action: Url,
    pub post: bool,
    // AUDIT FIX: enctype=multipart/form-data and method=dialog were ignored.
    pub multipart: bool,
    pub dialog: bool,
    pub id: Option<String>,
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
    /// True if the page hit [`MAX_ITEMS`] / depth limits and was simplified.
    pub truncated: bool,
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

    // AUDIT FIX: text/plain responses were parsed as HTML (tags vanished, whitespace collapsed).
    /// Render a `text/plain` document as preformatted text.
    pub fn plain_text(text: &str) -> Page {
        let mut b = Builder::new(Url::parse("about:blank").expect("static url"));
        for line in text.lines() {
            if b.items.len() >= MAX_ITEMS {
                b.truncated = true;
                break;
            }
            if !line.is_empty() {
                b.items.push(Item::Word {
                    text: line.replace('\t', "    "),
                    style: TextStyle::Mono,
                    link: None,
                    space_before: false,
                });
            }
            b.items.push(Item::Break);
        }
        b.finish(None)
    }
}

/// Maximum element nesting passed to the HTML parser.
const MAX_PARSE_DEPTH: usize = 1500;

/// Drop opening tags that would nest deeper than `max_depth` (approximate, linear scan).
// AUDIT FIX: html5ever's tree builder is O(depth²) (scope checks walk the open-element stack):
// 20k nested <div>s took ~1.7s and 100k froze the UI for ~40s. Real pages nest a few hundred
// levels at most, so excess opening tags are removed before parsing. Void, auto-closing and
// self-closing tags don't count; comments and raw-text elements (script/style/…) are skipped.
fn limit_nesting(html: &str, max_depth: usize) -> std::borrow::Cow<'_, str> {
    const VOID: &[&str] = &[
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr",
        // auto-closing: their siblings close them, so they don't really nest
        "p", "li", "dt", "dd", "option", "optgroup", "tr", "td", "th", "rb", "rt", "rp", "html", "head", "body",
    ];
    const RAW: &[&str] = &["script", "style", "textarea", "title", "xmp", "plaintext", "iframe", "noembed"];
    let bytes = html.as_bytes();
    let (mut i, mut depth, mut out, mut copied) = (0, 0usize, None::<String>, 0);
    let find = |from: usize, pat: &[u8]| bytes[from..].windows(pat.len()).position(|w| w.eq_ignore_ascii_case(pat)).map(|p| from + p);
    while let Some(off) = bytes[i..].iter().position(|&c| c == b'<') {
        let lt = i + off;
        let rest = &bytes[lt + 1..];
        if rest.starts_with(b"!--") {
            i = find(lt + 4, b"-->").map_or(bytes.len(), |p| p + 3);
            continue;
        }
        if rest.first() == Some(&b'/') {
            depth = depth.saturating_sub(1);
            i = lt + 2;
            continue;
        }
        if !rest.first().is_some_and(u8::is_ascii_alphabetic) {
            i = lt + 1;
            continue;
        }
        let name_len = rest.iter().position(|c| !c.is_ascii_alphanumeric() && *c != b'-').unwrap_or(rest.len());
        let name = std::str::from_utf8(&rest[..name_len]).unwrap_or("").to_ascii_lowercase();
        // End of the tag, honouring quoted attribute values.
        let mut j = lt + 1 + name_len;
        let mut quote = None;
        while j < bytes.len() {
            match (quote, bytes[j]) {
                (None, b'"' | b'\'') => quote = Some(bytes[j]),
                (Some(q), c) if c == q => quote = None,
                (None, b'>') => break,
                _ => {}
            }
            j += 1;
        }
        let end = (j + 1).min(bytes.len());
        let self_closing = j > 0 && bytes.get(j - 1) == Some(&b'/');
        if RAW.contains(&name.as_str()) {
            let close = format!("</{name}");
            i = find(end, close.as_bytes()).unwrap_or(bytes.len());
            continue;
        }
        if !self_closing && !VOID.contains(&name.as_str()) {
            if depth >= max_depth {
                let o = out.get_or_insert_with(|| String::with_capacity(html.len()));
                o.push_str(&html[copied..lt]); // '<' and '>' are ASCII: valid char boundaries
                copied = end;
            } else {
                depth += 1;
            }
        }
        i = end;
    }
    match out {
        None => std::borrow::Cow::Borrowed(html),
        Some(mut o) => {
            o.push_str(&html[copied..]);
            std::borrow::Cow::Owned(o)
        }
    }
}

pub fn parse(html: &str, url: &Url) -> anyhow::Result<Page> {
    let limited = limit_nesting(html, MAX_PARSE_DEPTH);
    let html: &str = &limited;
    let mut opts = ParseOpts::default();
    // We don't run JS against the DOM, so render <noscript> content like a no-JS browser.
    opts.tree_builder.scripting_enabled = false;
    let dom = parse_document(RcDom::default(), opts)
        .from_utf8()
        .read_from(&mut html.as_bytes())?;
    let mut b = Builder::new(url.clone());
    b.walk(&dom.document, Ctx::default(), 0);
    b.truncated |= matches!(limited, std::borrow::Cow::Owned(_));
    if b.truncated {
        b.gap();
        b.words("[Page too large or too deeply nested: the rest was simplified or omitted.]", TextStyle::Bold, None);
    }
    let title = b.title.take();
    let mut page = b.finish(title);
    page.resolve_form_owners();
    page.normalize_radio_groups();
    Ok(page)
}

impl Page {
    // AUDIT FIX: controls outside a <form> that use form="id" were never submitted.
    fn resolve_form_owners(&mut self) {
        let ids: HashMap<&str, usize> = self
            .forms
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.id.as_deref().map(|id| (id, i)))
            .collect();
        for c in &mut self.controls {
            if let Some(id) = c.form_attr.as_deref() {
                c.form = ids.get(id).copied();
            }
        }
    }

    // AUDIT FIX: several `checked` radios in one group were all submitted; browsers keep only the
    // last one checked.
    fn normalize_radio_groups(&mut self) {
        let mut last: HashMap<(Option<usize>, String), usize> = HashMap::new();
        for (i, c) in self.controls.iter().enumerate() {
            if let ControlKind::Radio { checked: true, .. } = c.kind {
                last.insert((c.form, c.name.clone()), i);
            }
        }
        for (i, c) in self.controls.iter_mut().enumerate() {
            if let ControlKind::Radio { checked, .. } = &mut c.kind {
                if *checked && !c.name.is_empty() {
                    *checked = last.get(&(c.form, c.name.clone())) == Some(&i);
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Ctx {
    style: TextStyle,
    link: Option<usize>,
    form: Option<usize>,
    pre: bool,
    // AUDIT FIX: controls inside <fieldset disabled> must be disabled (and not submitted).
    disabled: bool,
}

impl Default for Ctx {
    fn default() -> Self {
        Ctx { style: TextStyle::Normal, link: None, form: None, pre: false, disabled: false }
    }
}

struct Builder {
    base: Url,
    base_set: bool,
    title: Option<String>,
    items: Vec<Item>,
    links: Vec<Url>,
    forms: Vec<FormSpec>,
    controls: Vec<ControlSpec>,
    scripts: Vec<String>,
    styles: Vec<String>,
    pending_space: bool,
    truncated: bool,
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

/// Concatenated text of a subtree.
// AUDIT FIX: was recursive (stack overflow on deep trees); now uses an explicit stack.
fn text_content(h: &Handle) -> String {
    let mut out = String::new();
    let mut stack = vec![h.clone()];
    while let Some(n) = stack.pop() {
        if let NodeData::Text { contents } = &n.data {
            out.push_str(&contents.borrow());
        }
        stack.extend(n.children.borrow().iter().rev().cloned());
    }
    out
}

// AUDIT FIX: `str::split_whitespace` also splits on U+00A0, so `&nbsp;` collapsed and allowed
// line breaks. HTML whitespace is only ASCII space/tab/LF/FF/CR.
fn is_html_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0C' | '\r')
}

fn html_words(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_html_ws).filter(|w| !w.is_empty())
}

fn collapse_ws(s: &str) -> String {
    html_words(s).collect::<Vec<_>>().join(" ")
}

/// Parse a non-negative integer attribute leniently.
fn num_attr<T: std::str::FromStr>(h: &Handle, key: &str) -> Option<T> {
    attr(h, key).and_then(|v| v.trim().parse().ok())
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
            base_set: false,
            title: None,
            items: Vec::new(),
            links: Vec::new(),
            forms: Vec::new(),
            controls: Vec::new(),
            scripts: Vec::new(),
            styles: Vec::new(),
            pending_space: false,
            truncated: false,
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
            truncated: self.truncated,
        }
    }

    fn full(&mut self) -> bool {
        let full = self.items.len() >= MAX_ITEMS;
        self.truncated |= full;
        full
    }

    fn brk(&mut self) {
        self.pending_space = false;
        match self.items.last() {
            None | Some(Item::Break | Item::Gap | Item::Rule) => {}
            _ => self.items.push(Item::Break),
        }
    }

    fn gap(&mut self) {
        self.pending_space = false;
        match self.items.last() {
            None | Some(Item::Gap | Item::Rule) => {}
            Some(Item::Break) => {
                let n = self.items.len();
                self.items[n - 1] = Item::Gap;
            }
            _ => self.items.push(Item::Gap),
        }
    }

    /// Append whitespace-collapsed words.
    fn words(&mut self, s: &str, style: TextStyle, link: Option<usize>) {
        if s.starts_with(is_html_ws) {
            self.pending_space = true;
        }
        for w in html_words(s) {
            if self.full() {
                return;
            }
            self.items.push(Item::Word {
                text: w.to_string(),
                style,
                link,
                space_before: self.pending_space,
            });
            self.pending_space = true;
        }
        self.pending_space = s.ends_with(is_html_ws) || (s.is_empty() && self.pending_space);
    }

    fn push_control(&mut self, h: &Handle, ctx: Ctx, kind: ControlKind) {
        let hidden = matches!(kind, ControlKind::Hidden { .. });
        self.controls.push(ControlSpec {
            form: ctx.form,
            name: attr(h, "name").unwrap_or_default(),
            kind,
            disabled: ctx.disabled || has_attr(h, "disabled"),
            readonly: has_attr(h, "readonly"),
            autofocus: has_attr(h, "autofocus"),
            form_attr: attr(h, "form").filter(|f| !f.is_empty()),
        });
        if !hidden {
            let idx = self.controls.len() - 1;
            self.items.push(Item::Control { idx, space_before: self.pending_space });
            self.pending_space = true;
        }
    }

    fn resolve(&self, href: &str) -> Option<Url> {
        self.base.join(href.trim()).ok()
    }

    fn walk(&mut self, h: &Handle, ctx: Ctx, depth: usize) {
        if self.full() {
            return;
        }
        match &h.data {
            NodeData::Document => {
                for c in h.children.borrow().iter() {
                    self.walk(c, ctx, depth + 1);
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
            NodeData::Element { .. } if depth > MAX_DEPTH => {
                // AUDIT FIX: flatten absurdly deep subtrees to text instead of recursing further.
                self.truncated = true;
                let t = text_content(h);
                self.words(&t, ctx.style, ctx.link);
            }
            NodeData::Element { .. } => self.element(h, ctx, depth),
            _ => {}
        }
    }

    fn children(&mut self, h: &Handle, ctx: Ctx, depth: usize) {
        for c in h.children.borrow().iter() {
            self.walk(c, ctx, depth + 1);
        }
    }

    #[allow(clippy::too_many_lines)] // one arm per element kind reads best as a single match
    fn element(&mut self, h: &Handle, mut ctx: Ctx, depth: usize) {
        let tag = local_name(h).unwrap_or_default();
        let tag = tag.as_str();
        if let Some(id) = attr(h, "id").filter(|i| !i.is_empty()) {
            self.items.push(Item::Anchor(id));
        }
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
                // Only classic inline scripts (type=module / JSON / templates aren't JS to run).
                let ty = attr(h, "type").unwrap_or_default().to_ascii_lowercase();
                let classic = ty.is_empty() || ty.contains("javascript") || ty == "text/ecmascript";
                if !s.trim().is_empty() && attr(h, "src").is_none() && classic {
                    self.scripts.push(s);
                }
                return;
            }
            "style" => {
                self.styles.push(text_content(h));
                return;
            }
            "base" => {
                // AUDIT FIX: only the first <base href> applies (later ones were overriding it).
                if !self.base_set {
                    if let Some(u) = attr(h, "href").and_then(|v| self.resolve(&v)) {
                        self.base = u;
                        self.base_set = true;
                    }
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
                    self.words(&format!(" [{alt}] "), ctx.style, ctx.link);
                }
                return;
            }
            "input" => return self.input(h, ctx),
            "button" => {
                let kind = attr(h, "type").unwrap_or_default().trim().to_ascii_lowercase();
                let label = collapse_ws(&text_content(h));
                let label = if label.is_empty() { "Submit".to_string() } else { label };
                // AUDIT FIX: only type=button/reset are inert; missing *or invalid* types submit.
                let k = if matches!(kind.as_str(), "button" | "reset") {
                    ControlKind::Button { label }
                } else {
                    ControlKind::Submit {
                        value: attr(h, "value").unwrap_or_default(),
                        label,
                        image: false,
                        overrides: self.overrides(h),
                    }
                };
                self.push_control(h, ctx, k);
                return;
            }
            "textarea" => {
                let value = text_content(h);
                let value = value.strip_prefix('\n').unwrap_or(&value).to_string();
                let k = ControlKind::TextArea {
                    value,
                    rows: num_attr(h, "rows").unwrap_or(3),
                    cols: num_attr(h, "cols").unwrap_or(40),
                };
                self.push_control(h, ctx, k);
                return;
            }
            "select" => {
                let mut options = Vec::new();
                let mut selected = Vec::new();
                collect_options(h, &mut options, &mut selected);
                let multiple = has_attr(h, "multiple");
                if !multiple {
                    // Single select: last `selected` wins, else the first enabled option.
                    selected = selected
                        .last()
                        .copied()
                        .or_else(|| options.iter().position(|o| !o.disabled))
                        .into_iter()
                        .collect();
                }
                let k = ControlKind::Select { options, selected, multiple, size: num_attr(h, "size") };
                self.push_control(h, ctx, k);
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
                if let Some(name) = attr(h, "name").filter(|n| !n.is_empty()) {
                    self.items.push(Item::Anchor(name));
                }
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
            "fieldset" => ctx.disabled |= has_attr(h, "disabled"),
            "form" => {
                // AUDIT FIX: nested <form> start tags are dropped by the HTML parser, but a form
                // reached via foreign content could still nest; the innermost form owns its controls.
                let action = attr(h, "action")
                    .filter(|a| !a.trim().is_empty())
                    .and_then(|a| self.resolve(&a))
                    .unwrap_or_else(|| self.base.clone());
                let method = attr(h, "method").unwrap_or_default().trim().to_ascii_lowercase();
                let enctype = attr(h, "enctype").unwrap_or_default().trim().to_ascii_lowercase();
                self.forms.push(FormSpec {
                    action,
                    post: method == "post",
                    multipart: enctype == "multipart/form-data",
                    dialog: method == "dialog",
                    id: attr(h, "id").filter(|i| !i.is_empty()),
                });
                ctx.form = Some(self.forms.len() - 1);
            }
            _ => {}
        }

        self.children(h, ctx, depth);

        if matches!(tag, "td" | "th") {
            self.pending_space = true;
        }
        if is_gap {
            self.gap();
        } else if is_block {
            self.brk();
        }
    }

    fn overrides(&self, h: &Handle) -> SubmitOverrides {
        SubmitOverrides {
            action: attr(h, "formaction").filter(|a| !a.trim().is_empty()).and_then(|a| self.resolve(&a)),
            post: attr(h, "formmethod").map(|m| m.trim().eq_ignore_ascii_case("post")),
            multipart: attr(h, "formenctype").map(|e| e.trim().eq_ignore_ascii_case("multipart/form-data")),
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
                ControlKind::Submit {
                    value: value.unwrap_or_else(|| label.clone()),
                    label,
                    image: false,
                    overrides: self.overrides(h),
                }
            }
            "image" => ControlKind::Submit {
                value: String::new(),
                label: attr(h, "alt").filter(|a| !a.is_empty()).unwrap_or_else(|| "Submit".into()),
                image: true,
                overrides: self.overrides(h),
            },
            "button" | "reset" => ControlKind::Button {
                label: value.unwrap_or_else(|| if ty == "reset" { "Reset".into() } else { String::new() }),
            },
            "file" => ControlKind::Button { label: "(file upload unsupported)".into() },
            _ => ControlKind::Text {
                value: value.unwrap_or_default(),
                password: ty == "password",
                size: num_attr(h, "size"),
                maxlength: num_attr(h, "maxlength"),
            },
        };
        self.push_control(h, ctx, kind);
    }
}

fn collect_options(h: &Handle, opts: &mut Vec<SelectOption>, sel: &mut Vec<usize>) {
    for c in h.children.borrow().iter() {
        match local_name(c).as_deref() {
            Some("option") => {
                let label = collapse_ws(&text_content(c));
                let value = attr(c, "value").unwrap_or_else(|| label.clone());
                if has_attr(c, "selected") {
                    sel.push(opts.len());
                }
                opts.push(SelectOption { label, value, disabled: has_attr(c, "disabled") });
            }
            Some("optgroup") => collect_options(c, opts, sel),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://e.x/dir/page").unwrap()
    }

    fn words(p: &Page) -> Vec<(&str, bool)> {
        p.items
            .iter()
            .filter_map(|i| match i {
                Item::Word { text, space_before, .. } => Some((text.as_str(), *space_before)),
                _ => None,
            })
            .collect()
    }

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
        assert!(matches!(&p.controls[2].kind, ControlKind::Select { selected, .. } if selected == &[1]));
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
        let p = parse("<p>foo<b>bar</b> baz</p>", &url()).unwrap();
        assert_eq!(words(&p), [("foo", false), ("bar", false), ("baz", true)]);
    }

    #[test]
    fn nbsp_is_not_collapsible_whitespace() {
        let p = parse("<p>a&nbsp;b &nbsp;c</p>", &url()).unwrap();
        assert_eq!(words(&p), [("a\u{a0}b", false), ("\u{a0}c", true)]);
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        // Run on a small stack to prove the walk is bounded.
        let html = format!("<body>{}deep{}</body>", "<b><i>".repeat(20_000), "</i></b>".repeat(20_000));
        let h = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let p = parse(&html, &url()).unwrap();
                (p.truncated, words(&p).iter().any(|(w, _)| *w == "deep"))
            })
            .unwrap();
        assert_eq!(h.join().unwrap(), (true, true));
    }

    #[test]
    fn nesting_limiter() {
        let ok = "<div><p>a<li>b<br><img src=x><script>if (a<b) {}</script><!-- <div> --></div>";
        assert!(matches!(limit_nesting(ok, 3), std::borrow::Cow::Borrowed(_)));
        let deep = format!("{}x{}", "<div title='a>b'>".repeat(10), "</div>".repeat(10));
        let lim = limit_nesting(&deep, 3);
        assert_eq!(lim.matches("<div").count(), 3);
        assert!(lim.contains('x'));
        // multibyte text around dropped tags stays valid
        let mb = format!("{}é😀{}", "<span>é".repeat(5), "</span>".repeat(5));
        assert_eq!(limit_nesting(&mb, 2).matches("<span>").count(), 2);
        // 100k nested divs: parses fast now
        let t = std::time::Instant::now();
        let html = format!("{}deep{}", "<div>".repeat(100_000), "</div>".repeat(100_000));
        let p = parse(&html, &url()).unwrap();
        assert!(p.truncated && words(&p).iter().any(|(w, _)| *w == "deep"));
        assert!(t.elapsed().as_secs() < 10, "{:?}", t.elapsed());
    }

    #[test]
    fn huge_page_is_capped() {
        let html = format!("<p>{}</p>", "w ".repeat(MAX_ITEMS + 10));
        let p = parse(&html, &url()).unwrap();
        assert!(p.truncated);
        assert!(p.items.len() <= MAX_ITEMS + 4);
    }

    #[test]
    fn url_resolution_and_base() {
        let html = r##"<head><base href="https://cdn.x/a/b/"><base href="https://evil.x/"></head>
            <a href="../up">u</a><a href="#frag">f</a><a href="javascript:alert(1)">j</a>
            <a href="mailto:a@b.c">m</a><a href="  /abs  ">s</a>"##;
        let p = parse(html, &url()).unwrap();
        let links: Vec<_> = p.links.iter().map(Url::as_str).collect();
        assert_eq!(
            links,
            ["https://cdn.x/a/up", "https://cdn.x/a/b/#frag", "mailto:a@b.c", "https://cdn.x/abs"]
        );
    }

    #[test]
    fn form_edge_cases() {
        let html = r#"<form id=f action=/go method=POST enctype="multipart/form-data">
            <input name=t maxlength=5 readonly autofocus>
            <input type=radio name=r value=1 checked><input type=radio name=r value=2 checked>
            <select name=s><option disabled>x</option><option>y</option></select>
            <select name=m multiple><option selected>a</option><option selected>b</option></select>
            <fieldset disabled><input name=fd></fieldset>
            <button type=bogus name=b value=v formaction=/alt formmethod=get>B</button>
            <input type=image name=img>
            </form><input form=f name=outside><input form=nope name=orphan>
            <form method=dialog></form>"#;
        let p = parse(html, &url()).unwrap();
        assert!(p.forms[0].post && p.forms[0].multipart && !p.forms[0].dialog);
        assert!(p.forms[1].dialog);
        let by = |n: &str| p.controls.iter().find(|c| c.name == n).unwrap();
        assert!(matches!(by("t").kind, ControlKind::Text { maxlength: Some(5), .. }));
        assert!(by("t").readonly && by("t").autofocus);
        let radios: Vec<bool> = p
            .controls
            .iter()
            .filter_map(|c| match c.kind {
                ControlKind::Radio { checked, .. } => Some(checked),
                _ => None,
            })
            .collect();
        assert_eq!(radios, [false, true]);
        assert!(matches!(&by("s").kind, ControlKind::Select { selected, .. } if selected == &[1]));
        assert!(matches!(&by("m").kind, ControlKind::Select { selected, multiple: true, .. } if selected == &[0, 1]));
        assert!(by("fd").disabled);
        match &by("b").kind {
            ControlKind::Submit { overrides, .. } => {
                assert_eq!(overrides.action.as_ref().unwrap().as_str(), "https://e.x/alt");
                assert_eq!(overrides.post, Some(false));
            }
            k => panic!("{k:?}"),
        }
        assert!(matches!(by("img").kind, ControlKind::Submit { image: true, .. }));
        assert_eq!(by("outside").form, Some(0));
        assert_eq!(by("orphan").form, None);
    }

    #[test]
    fn anchors_and_plain_text() {
        let p = parse("<h2 id=top>T</h2><a name=old>x</a>", &url()).unwrap();
        let anchors: Vec<_> = p
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Anchor(a) => Some(a.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(anchors, ["top", "old"]);
        let t = Page::plain_text("<b>x</b>\n  y");
        assert_eq!(words(&t), [("<b>x</b>", false), ("  y", false)]);
    }

    #[test]
    fn malformed_and_empty_inputs_do_not_panic() {
        for html in ["", "<", "<<<>>>", "<p", "</p></p></div>", "\0\0", "<table><td><form><td>x", "&#xFFFFFF;&#0;"] {
            parse(html, &url()).unwrap();
        }
    }
}
