//! Mini Ladybird — a from-scratch cross-platform browser prototype
//! Inspired by Ladybird's multi-process, standards-first architecture.
//!
//! Architecture mapping (single-process for the prototype; modular for future isolation):
//! - UI layer: FLTK (address bar, navigation, scrollable content view)
//! - Network (`net`): reqwest (HTTP/1.1 + HTTP/2, rustls, cookies, compression)
//! - HTML (`page`): html5ever (WHATWG parser) → `RcDom` → inline flow of words + form controls
//! - Layout/paint (`layout`): word-wrapping inline layout, re-run on resize
//! - CSS: lightningcss (parsed, not yet applied)
//! - JavaScript: quickjs-ng via quickjs-rusty
//!
//! Security notes:
//! - Network calls have timeouts and a capped body size
//! - No arbitrary file access from page content; only http(s) navigation
//! - Scripts run in an embedded engine with limited host bindings

mod layout;
mod net;
mod page;
mod scrolling;

use anyhow::{Context, Result};
use fltk::{
    app,
    browser::MultiBrowser,
    button::{Button, CheckButton, RoundButton},
    enums::{Align, CallbackTrigger, Color, Cursor, Event, Font, FrameType},
    frame::Frame,
    group::{Flex, Scroll, ScrollType},
    input::{Input, MultilineInput, SecretInput},
    menu::Choice,
    prelude::*,
    widget::Widget,
    window::Window,
};
use layout::Layout;
use lightningcss::stylesheet::{ParserOptions, StyleSheet};
use net::Request;
use page::{ControlKind, Page};
use quickjs_rusty::Context as JsContext;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use url::Url;

// AUDIT FIX: unbounded session history (each entry may hold a POST body).
const MAX_HISTORY: usize = 100;
// AUDIT FIX: page scripts ran with no time/memory limit — `while(true){}` froze the UI forever.
const JS_TIME_LIMIT: Duration = Duration::from_millis(1500);
const JS_MEMORY_LIMIT: usize = 32 * 1024 * 1024;
/// Max `alert()` dialogs per page (a page looping on `alert()` was an endless modal loop).
const MAX_ALERTS_PER_PAGE: u32 = 3;
/// Search used when the address bar text isn't a URL.
const SEARCH_URL: &str = "https://lite.duckduckgo.com/lite/";

// AUDIT FIX: page-bound messages carry the page generation they were created for. Messages are
// queued, so a double-click on a link (or a radio click racing a navigation) used to index into the
// *next* page's links/controls: wrong navigation, or an out-of-bounds panic in radio_toggled.
#[derive(Debug, Clone, Copy)]
enum Msg {
    /// Navigate to whatever is in the address bar.
    Go,
    Back,
    /// Follow link `idx` of page generation `page_gen`.
    Link { page_gen: u64, idx: usize },
    /// Submit form `form`, optionally via the submit button control `submitter`.
    Submit { page_gen: u64, form: usize, submitter: Option<usize> },
    /// A radio button was toggled: enforce exclusivity within its group.
    Radio { page_gen: u64, idx: usize },
    Relayout,
}

/// A live FLTK widget for a form control.
enum Ctl {
    Text(Input),
    Secret(SecretInput),
    Area(MultilineInput),
    Check(CheckButton),
    Radio(RoundButton),
    Button(Button),
    Choice(Choice),
    // AUDIT FIX: <select multiple>/size>1 rendered as a list box (multi-selection was impossible).
    List(MultiBrowser),
}

impl Ctl {
    fn base(&self) -> Widget {
        match self {
            Ctl::Text(w) => w.as_base_widget(),
            Ctl::Secret(w) => w.as_base_widget(),
            Ctl::Area(w) => w.as_base_widget(),
            Ctl::Check(w) => w.as_base_widget(),
            Ctl::Radio(w) => w.as_base_widget(),
            Ctl::Button(w) => w.as_base_widget(),
            Ctl::Choice(w) => w.as_base_widget(),
            Ctl::List(w) => w.as_base_widget(),
        }
    }
}

struct Browser {
    win: Window,
    url_input: Input,
    status: Frame,
    scroll: Scroll,
    canvas: Frame,
    client: reqwest::blocking::Client,
    js_ctx: JsContext,
    sender: app::Sender<Msg>,
    page: Page,
    current: Option<Request>,
    history: Vec<Request>,
    widgets: Vec<Option<Ctl>>,
    layout: Rc<RefCell<Layout>>,
    /// Incremented on every page change; see [`Msg`].
    page_gen: u64,
    page_gen_cell: Rc<std::cell::Cell<u64>>,
}

#[allow(clippy::too_many_lines)] // linear UI construction; splitting it hurts readability
fn main() -> Result<()> {
    env_logger::init();
    let app = app::App::default().with_scheme(app::Scheme::Gtk);
    let (sender, receiver) = app::channel::<Msg>();

    let mut win = Window::new(100, 100, 960, 700, "Mini Ladybird — Rust + FLTK");
    win.set_color(Color::from_rgb(30, 30, 30));

    let mut col = Flex::default_fill().column();
    col.set_margin(8);
    col.set_pad(6);

    let mut bar = Flex::default().row();
    bar.set_pad(6);
    let mut back_btn = Button::default().with_label("@<-");
    back_btn.set_color(Color::from_rgb(60, 60, 60));
    back_btn.set_label_color(Color::White);
    let mut url_input = Input::default();
    url_input.set_value("https://duckduckgo.com/lite/");
    url_input.set_color(Color::from_rgb(50, 50, 50));
    url_input.set_text_color(Color::White);
    url_input.set_cursor_color(Color::White);
    url_input.set_text_font(Font::Courier);
    url_input.set_text_size(14);
    url_input.set_trigger(CallbackTrigger::EnterKeyAlways);
    let mut go_btn = Button::default().with_label("Go");
    go_btn.set_color(Color::from_rgb(70, 130, 180));
    go_btn.set_label_color(Color::White);
    bar.fixed(&back_btn, 36);
    bar.fixed(&go_btn, 70);
    bar.end();
    col.fixed(&bar, 30);

    let mut status = Frame::default().with_label("Ready");
    status.set_label_color(Color::from_rgb(180, 180, 180));
    status.set_align(Align::Left | Align::Inside | Align::Clip);
    status.set_frame(FrameType::FlatBox);
    status.set_color(Color::from_rgb(40, 40, 40));
    col.fixed(&status, 20);

    let mut scroll = Scroll::default();
    scroll.set_type(ScrollType::Vertical);
    scroll.set_color(layout::BG);
    scroll.set_frame(FrameType::FlatBox);
    let mut canvas = Frame::default();
    canvas.set_frame(FrameType::NoBox);
    scroll.end();
    // Mouse wheel: ~3 lines per notch instead of FLTK's 16px default.
    scroll.scrollbar().set_linesize(scrolling::LINE_STEP);
    // Scrollbars must not grab keyboard focus (they'd then eat page keys with their own rules).
    scroll.scrollbar().clear_visible_focus();
    scroll.hscrollbar().clear_visible_focus();
    col.end();
    win.end();
    win.resizable(&col);
    win.make_resizable(true);
    win.show();
    col.layout();

    // Text layer: draws the laid-out words; clicks on links navigate.
    let layout = Rc::new(RefCell::new(Layout::default()));
    // Shared with the canvas handler so link clicks are tagged with the current page generation.
    let page_gen_cell = Rc::new(std::cell::Cell::new(0_u64));
    {
        let l = layout.clone();
        let s = sender;
        canvas.draw(move |c| {
            let l = l.borrow();
            // Flex may resize the viewport lazily (without a window resize): re-layout then.
            if let Some(p) = c.parent() {
                if (p.w(), p.h()) != l.viewport {
                    s.send(Msg::Relayout);
                }
            }
            // Visible rows in layout coordinates (the scroll area clips the canvas).
            let (top, bottom) = c.parent().map_or((0, c.h()), |p| (p.y() - c.y(), p.y() - c.y() + p.h()));
            layout::paint(&l, c.x(), c.y(), c.w(), c.h(), top, bottom);
        });
    }
    {
        let l = layout.clone();
        let s = sender;
        let mut sc = scroll.clone();
        let page_gen = page_gen_cell.clone();
        canvas.handle(move |c, ev| {
            let hit = || layout::link_at(&l.borrow(), app::event_x() - c.x(), app::event_y() - c.y());
            match ev {
                Event::Move => {
                    let cur = if hit().is_some() { Cursor::Hand } else { Cursor::Default };
                    if let Some(mut w) = c.window() {
                        w.set_cursor(cur);
                    }
                    true
                }
                Event::Leave => {
                    if let Some(mut w) = c.window() {
                        w.set_cursor(Cursor::Default);
                    }
                    true
                }
                // The page itself can hold focus (like a browser's document), so clicking
                // text leaves a form field and Space/arrows scroll the page again.
                Event::Enter | Event::Focus | Event::Unfocus => true,
                // Handle page keys here, before FLTK's group arrow-key focus navigation.
                Event::KeyDown => page_key(&mut sc, &l),
                Event::Push => {
                    if hit().is_none() {
                        let _ = c.take_focus();
                    }
                    true
                }
                Event::Released => {
                    if let Some(idx) = hit() {
                        s.send(Msg::Link { page_gen: page_gen.get(), idx });
                    }
                    true
                }
                _ => false,
            }
        });
    }

    // Page keyboard scrolling regardless of focus: FLTK delivers keys to the focused
    // widget first, so every focusable widget (and the window, for the rest) gets a
    // pre-handler that runs before FLTK's default handling.
    // With nothing focused, FLTK offers the key to the window as a Shortcut event.
    {
        win.super_handle_first(false);
        let mut sc = scroll.clone();
        let l = layout.clone();
        win.handle(move |_, ev| matches!(ev, Event::KeyDown | Event::Shortcut) && page_key(&mut sc, &l));
    }
    intercept_page_keys(&mut url_input, &scroll, &layout);
    intercept_page_keys(&mut back_btn, &scroll, &layout);
    intercept_page_keys(&mut go_btn, &scroll, &layout);

    back_btn.emit(sender, Msg::Back);
    go_btn.emit(sender, Msg::Go);
    url_input.emit(sender, Msg::Go);
    win.resize_callback(move |_, _, _, _, _| sender.send(Msg::Relayout));

    let js_ctx = new_js_context()?;

    let mut b = Browser {
        win,
        url_input,
        status,
        scroll,
        canvas,
        client: net::client()?,
        js_ctx,
        sender,
        page: Page::message("Mini Ladybird", "Type a URL and press Enter or Go."),
        current: None,
        history: Vec::new(),
        widgets: Vec::new(),
        layout,
        page_gen: 0,
        page_gen_cell,
    };
    b.show_page();

    // Optional start URL from argv.
    if let Some(u) = std::env::args().nth(1) {
        b.url_input.set_value(&u);
        b.sender.send(Msg::Go);
    }

    while app.wait() {
        if let Some(msg) = receiver.recv() {
            b.handle(msg);
        }
    }
    Ok(())
}

/// Wall-clock deadline (ms since process start) checked by the `QuickJS` interrupt handler.
static JS_DEADLINE_MS: AtomicU64 = AtomicU64::new(u64::MAX);
static JS_ALERTS: AtomicU32 = AtomicU32::new(0);

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// `QuickJS` calls this periodically while running; non-zero aborts the script.
unsafe extern "C" fn js_interrupt(_: *mut quickjs_rusty::JSContext, _: *mut std::ffi::c_void) -> std::ffi::c_int {
    std::ffi::c_int::from(now_ms() > JS_DEADLINE_MS.load(Ordering::Relaxed))
}

// AUDIT FIX: one QuickJS context was shared by every page, so globals defined by site A were
// readable by site B (cross-origin state leak). A fresh, memory-capped, interruptible context is
// now created per page.
fn new_js_context() -> Result<JsContext> {
    let ctx = JsContext::builder()
        .memory_limit(JS_MEMORY_LIMIT)
        .build()
        .context("Failed to create QuickJS context")?;
    ctx.set_interrupt_handler(Some(js_interrupt), std::ptr::null_mut());
    ctx.add_callback("alert", |msg: String| {
        if JS_ALERTS.fetch_add(1, Ordering::Relaxed) < MAX_ALERTS_PER_PAGE {
            fltk::dialog::alert_default(&msg);
        }
        true
    })?;
    ctx.add_callback("consoleLog", |msg: String| {
        eprintln!("[JS] {msg}");
        true
    })?;
    Ok(ctx)
}

/// FLTK labels interpret `@` as a symbol prefix (`@->` draws an arrow); escape it for literal text.
// AUDIT FIX: URLs such as https://x.com/@user rendered garbage in the status bar / buttons / menus.
fn label_escape(s: &str) -> String {
    s.replace('@', "@@")
}

/// Address-bar text → URL. Text that doesn't look like a URL becomes a web search.
// AUDIT FIX: typing words ("rust fltk") produced "Invalid URL" (https://rust fltk).
fn omnibox_url(raw: &str) -> Option<Url> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.contains("://") {
        return Url::parse(raw).ok();
    }
    let looks_like_host = !raw.contains(char::is_whitespace)
        && (raw.contains('.') || raw.starts_with("localhost") || raw.contains(':'));
    if looks_like_host {
        if let Ok(u) = Url::parse(&format!("https://{raw}")) {
            return Some(u);
        }
    }
    let mut u = Url::parse(SEARCH_URL).ok()?;
    u.query_pairs_mut().append_pair("q", raw);
    Some(u)
}

/// Run [`page_key`] before `w`'s own key handling.
fn intercept_page_keys<W: WidgetBase>(w: &mut W, scroll: &Scroll, layout: &Rc<RefCell<Layout>>) {
    w.super_handle_first(false);
    let mut sc = scroll.clone();
    let l = layout.clone();
    w.handle(move |_, ev| ev == Event::KeyDown && page_key(&mut sc, &l));
}

/// If the current key should scroll the page (given what has focus), scroll and consume it.
fn page_key(sc: &mut Scroll, layout: &Rc<RefCell<Layout>>) -> bool {
    if app::grab().is_some() {
        return false; // a popup menu (e.g. an open <select>) owns the keyboard
    }
    let focus = app::focus().map_or(scrolling::Focus::Page, |w| classify_focus(&w.as_base_widget()));
    let Some(action) = scrolling::action_for(app::event_key(), app::is_event_shift(), focus) else {
        return false;
    };
    let content_h = layout.borrow().height;
    let cur = sc.yposition();
    let to = scrolling::target(action, cur, content_h, sc.h());
    log::debug!("scroll {action:?} focus={focus:?} y {cur} -> {to} (content {content_h}, view {})", sc.h());
    if to != cur {
        sc.scroll_to(0, to);
        sc.redraw();
    }
    true
}

/// Classify the focused widget for keyboard-scroll decisions.
fn classify_focus(w: &Widget) -> scrolling::Focus {
    use fltk::utils::is_ptr_of;
    use scrolling::Focus;
    let p = w.as_widget_ptr();
    if is_ptr_of::<MultilineInput>(p) {
        // Does the text overflow the box (so the textarea needs its own PageUp/Down)?
        let ta = unsafe { MultilineInput::from_widget_ptr(p) };
        fltk::draw::set_font(ta.text_font(), ta.text_size());
        let inner_w = f64::from((ta.w() - 8).max(1));
        // AUDIT FIX: summed in f64 (no i32 truncation/overflow for huge pasted text).
        let lines: f64 = ta.value().split('\n').map(|line| (fltk::draw::width(line) / inner_w).ceil().max(1.0)).sum();
        Focus::TextArea { scrollable: lines * f64::from(fltk::draw::height()) > f64::from(ta.h() - 4) }
    } else if is_ptr_of::<Input>(p) || is_ptr_of::<SecretInput>(p) {
        Focus::TextField
    } else if is_ptr_of::<Choice>(p) || is_ptr_of::<MultiBrowser>(p) {
        Focus::Choice
    } else if is_ptr_of::<Button>(p) || is_ptr_of::<CheckButton>(p) || is_ptr_of::<RoundButton>(p) {
        Focus::Button
    } else {
        Focus::Page
    }
}

/// `maxlength` / `readonly` for text widgets.
fn apply_text_attrs<I: InputExt>(w: &mut I, maxlength: Option<usize>, readonly: bool) {
    if let Some(n) = maxlength.and_then(|n| i32::try_from(n).ok()) {
        w.set_maximum_size(n);
    }
    w.set_readonly(readonly);
}

impl Browser {
    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Go => match omnibox_url(&self.url_input.value()) {
                Some(u) => self.load(Request::get(u), true),
                None => self.set_status("Type a URL or search terms"),
            },
            Msg::Back => {
                let Some(prev) = self.history.pop() else {
                    self.set_status("No previous page");
                    return;
                };
                // AUDIT FIX: Back silently re-sent POST bodies (duplicate orders/messages). Ask first.
                if prev.form_body.is_some() {
                    let answer = fltk::dialog::choice2_default(
                        "The previous page was the result of a form submission.\nResend the form data?",
                        "Cancel",
                        "Resend",
                        "",
                    );
                    if answer != Some(1) {
                        self.history.push(prev);
                        return;
                    }
                }
                self.load(prev, false);
            }
            Msg::Link { page_gen, idx } if page_gen == self.page_gen => {
                if let Some(u) = self.page.links.get(idx).cloned() {
                    if self.is_same_document_fragment(&u) {
                        self.scroll_to_fragment(u.fragment().unwrap_or(""));
                        return;
                    }
                    let referer = self.current.as_ref().map(|r| r.url.clone());
                    self.load(Request { url: u, form_body: None, referer }, true);
                }
            }
            Msg::Submit { page_gen, form, submitter } if page_gen == self.page_gen => {
                match self.form_submission(form, submitter) {
                    Some(req) => self.load(req, true),
                    None => self.set_status("This form can't be submitted (method=dialog or invalid)"),
                }
            }
            Msg::Radio { page_gen, idx } if page_gen == self.page_gen => self.radio_toggled(idx),
            Msg::Link { .. } | Msg::Submit { .. } | Msg::Radio { .. } => {
                log::debug!("dropping stale message {msg:?}");
            }
            Msg::Relayout => {
                if (self.scroll.w(), self.scroll.h()) != self.layout.borrow().viewport {
                    self.relayout();
                }
            }
        }
    }

    fn is_same_document_fragment(&self, u: &Url) -> bool {
        self.current.as_ref().is_some_and(|cur| {
            u.fragment().is_some()
                && cur.url[..url::Position::AfterQuery] == u[..url::Position::AfterQuery]
        })
    }

    fn set_status(&mut self, text: &str) {
        self.status.set_label(&label_escape(text));
    }

    fn push_history(&mut self) {
        if let Some(cur) = self.current.take() {
            self.history.push(cur);
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
    }

    fn load(&mut self, req: Request, push_history: bool) {
        if !matches!(req.url.scheme(), "http" | "https") {
            self.set_status(&format!("Can't open {}: links ({})", req.url.scheme(), req.url));
            return;
        }
        let verb = if req.form_body.is_some() { "POST" } else { "GET" };
        self.set_status(&format!("{verb} {} …", req.url));
        self.win.set_cursor(Cursor::Wait);
        app::flush();

        let result = net::fetch(&self.client, &req);
        self.win.set_cursor(Cursor::Default);
        if push_history {
            self.push_history();
        }
        match result {
            Ok(resp) => {
                // AUDIT FIX: everything was parsed as HTML — images/binaries became screens of
                // garbage text and text/plain lost its line breaks.
                let is_html = resp.mime.is_empty() || resp.mime.contains("html") || resp.mime.ends_with("xml");
                let page = if is_html {
                    page::parse(&resp.body, &resp.url).unwrap_or_else(|e| Page::message("Parse error", &e.to_string()))
                } else if resp.mime.starts_with("text/") || resp.mime == "application/json" {
                    Page::plain_text(&resp.body)
                } else {
                    Page::message(
                        "Can't display this content",
                        &format!("{} is {} ({} bytes). Only HTML and text are supported.", resp.url, resp.mime, resp.body.len()),
                    )
                };
                self.url_input.set_value(resp.url.as_str());
                // Keep the method/body so Back can re-show POST results (e.g. DDG lite) — unless the
                // server redirected (POST → 303 → GET), in which case the document is a plain GET.
                let form_body = if resp.redirected { None } else { req.form_body };
                // AUDIT FIX: the status bar said "POST 200 /after" for a POST→303→GET result.
                let verb = if form_body.is_some() { "POST" } else { "GET" };
                self.current = Some(Request { url: resp.url.clone(), form_body, referer: req.referer });
                self.page = page;
                if is_html {
                    self.run_page_resources();
                }
                self.show_page();
                if let Some(frag) = resp.url.fragment() {
                    self.scroll_to_fragment(frag);
                }
                let note = if resp.truncated || self.page.truncated { " — page truncated (too large)" } else { "" };
                self.set_status(&format!(
                    "{verb} {} {} — {} links, {} form(s){note}",
                    resp.status,
                    resp.url,
                    self.page.links.len(),
                    self.page.forms.len()
                ));
            }
            Err(e) => {
                // AUDIT FIX: the failed request becomes the current entry, so Back returns to the
                // page you were on (previously Back skipped it) and Go/Enter retries.
                self.current = Some(req.clone());
                self.page = Page::message("Navigation failed", &format!("{}\n{:#}", req.url, e));
                self.show_page();
                self.set_status(&format!("Error: {e:#}"));
            }
        }
    }

    /// Scroll so the element with this id / name is at the top (`#` or `#top` → page top).
    fn scroll_to_fragment(&mut self, frag: &str) {
        let decoded = url::form_urlencoded::parse(format!("x={frag}").as_bytes())
            .next()
            .map_or_else(|| frag.to_string(), |(_, v)| v.into_owned());
        let y = {
            let l = self.layout.borrow();
            l.anchors.get(frag).or_else(|| l.anchors.get(&decoded)).copied().or_else(|| {
                (frag.is_empty() || frag.eq_ignore_ascii_case("top")).then_some(0)
            })
        };
        match y {
            Some(y) => {
                let max = (self.layout.borrow().height - self.scroll.h()).max(0);
                self.scroll.scroll_to(0, y.clamp(0, max));
                self.scroll.redraw();
            }
            None => self.set_status(&format!("No anchor #{frag} on this page")),
        }
    }

    fn run_page_resources(&mut self) {
        for css in &self.page.styles {
            match StyleSheet::parse(css, ParserOptions::default()) {
                Ok(sheet) => log::info!("Parsed CSS stylesheet with {} rules", sheet.rules.0.len()),
                Err(e) => log::warn!("CSS parse error: {e:?}"),
            }
        }
        if self.page.scripts.is_empty() {
            return;
        }
        match new_js_context() {
            Ok(ctx) => self.js_ctx = ctx,
            Err(e) => {
                log::warn!("JS disabled for this page: {e}");
                return;
            }
        }
        JS_ALERTS.store(0, Ordering::Relaxed);
        for script in &self.page.scripts {
            JS_DEADLINE_MS.store(now_ms() + u64::try_from(JS_TIME_LIMIT.as_millis()).unwrap_or(1500), Ordering::Relaxed);
            if let Err(e) = self.js_ctx.eval(script, false) {
                log::warn!("JS error: {e}");
            }
        }
        JS_DEADLINE_MS.store(u64::MAX, Ordering::Relaxed);
    }

    /// Replace form widgets for the new page and lay everything out.
    #[allow(clippy::too_many_lines)] // one arm per control kind
    fn show_page(&mut self) {
        for w in self.widgets.drain(..).flatten() {
            let base = w.base();
            self.scroll.remove(&base);
            app::delete_widget(base);
        }
        self.page_gen += 1;
        self.page_gen_cell.set(self.page_gen);
        let page_gen = self.page_gen;
        let title = self.page.title.clone().unwrap_or_else(|| "(untitled)".into());
        self.win.set_label(&format!("{title} — Mini Ladybird"));

        self.scroll.begin();
        let s = self.sender;
        for (i, spec) in self.page.controls.iter().enumerate() {
            let form = spec.form;
            let submit = move |submitter: Option<usize>| form.map(|form| Msg::Submit { page_gen, form, submitter });
            let mut ctl = match &spec.kind {
                ControlKind::Hidden { .. } => None,
                ControlKind::Text { value, password, maxlength, .. } => {
                    // Enter in a text field submits its form (implicit submission).
                    let msg = submit(None);
                    if *password {
                        let mut w = SecretInput::default();
                        w.set_value(value);
                        apply_text_attrs(&mut w, *maxlength, spec.readonly);
                        w.set_trigger(CallbackTrigger::EnterKeyAlways);
                        if let Some(m) = msg {
                            w.set_callback(move |_| s.send(m));
                        }
                        Some(Ctl::Secret(w))
                    } else {
                        let mut w = Input::default();
                        w.set_value(value);
                        apply_text_attrs(&mut w, *maxlength, spec.readonly);
                        w.set_trigger(CallbackTrigger::EnterKeyAlways);
                        if let Some(m) = msg {
                            w.set_callback(move |_| s.send(m));
                        }
                        Some(Ctl::Text(w))
                    }
                }
                ControlKind::TextArea { value, .. } => {
                    let mut w = MultilineInput::default();
                    w.set_value(value);
                    w.set_wrap(true);
                    apply_text_attrs(&mut w, None, spec.readonly);
                    Some(Ctl::Area(w))
                }
                ControlKind::Checkbox { checked, .. } => {
                    let mut w = CheckButton::default();
                    w.set_value(*checked);
                    w.set_color(layout::BG);
                    w.set_selection_color(Color::from_rgb(120, 175, 255));
                    Some(Ctl::Check(w))
                }
                ControlKind::Radio { checked, .. } => {
                    let mut w = RoundButton::default();
                    w.set_value(*checked);
                    w.set_color(layout::BG);
                    w.set_selection_color(Color::from_rgb(120, 175, 255));
                    w.set_callback(move |_| s.send(Msg::Radio { page_gen, idx: i }));
                    Some(Ctl::Radio(w))
                }
                ControlKind::Submit { label, .. } => {
                    let mut w = Button::default().with_label(&label_escape(label));
                    if let Some(m) = submit(Some(i)) {
                        w.set_callback(move |_| s.send(m));
                    }
                    Some(Ctl::Button(w))
                }
                ControlKind::Button { label } => {
                    let mut w = Button::default().with_label(&label_escape(label));
                    w.deactivate();
                    Some(Ctl::Button(w))
                }
                ControlKind::Select { options, selected, multiple, size } => {
                    if *multiple || size.is_some_and(|n| n > 1) {
                        let mut w = MultiBrowser::default();
                        for o in options {
                            // "@." = print the rest literally (Fl_Browser format codes start with '@').
                            w.add(&format!("@.{}", o.label));
                        }
                        for &k in selected {
                            if let Ok(line) = i32::try_from(k + 1) {
                                w.select(line);
                            }
                        }
                        Some(Ctl::List(w))
                    } else {
                        let mut w = Choice::default();
                        for o in options {
                            let esc = o
                                .label
                                .replace('\\', "\\\\")
                                .replace('/', "\\/")
                                .replace('&', "&&")
                                .replace('|', " ")
                                .replace('@', "@@"); // AUDIT FIX: '@' is a symbol prefix in menu labels too
                            w.add_choice(if esc.is_empty() { " " } else { &esc });
                        }
                        // AUDIT FIX: `selected as i32` could wrap; use a checked conversion.
                        if let Some(k) = selected.first().and_then(|&k| i32::try_from(k).ok()) {
                            w.set_value(k);
                        }
                        Some(Ctl::Choice(w))
                    }
                }
            };
            if let Some(c) = &mut ctl {
                let (sc, l) = (&self.scroll, &self.layout);
                match c {
                    Ctl::Text(w) => intercept_page_keys(w, sc, l),
                    Ctl::Secret(w) => intercept_page_keys(w, sc, l),
                    Ctl::Area(w) => intercept_page_keys(w, sc, l),
                    Ctl::Check(w) => intercept_page_keys(w, sc, l),
                    Ctl::Radio(w) => intercept_page_keys(w, sc, l),
                    Ctl::Button(w) => intercept_page_keys(w, sc, l),
                    Ctl::Choice(w) => intercept_page_keys(w, sc, l),
                    Ctl::List(w) => intercept_page_keys(w, sc, l),
                }
                let mut base = c.base();
                base.set_label_size(13);
                if spec.disabled {
                    base.deactivate();
                }
            }
            self.widgets.push(ctl);
        }
        self.scroll.end();
        self.scroll.scroll_to(0, 0);
        self.relayout();
        // AUDIT FIX: focus only an `autofocus` control (was: always the first text field, which
        // swallowed Space/arrow keys meant to scroll); otherwise the page itself gets focus.
        let auto = self.page.controls.iter().position(|c| c.autofocus && !c.disabled);
        match auto.and_then(|i| self.widgets.get(i)).and_then(Option::as_ref) {
            Some(w) => {
                let _ = w.base().take_focus();
            }
            None => {
                let _ = self.canvas.take_focus();
            }
        }
    }

    /// Lay out for the current viewport width and position canvas + widgets.
    fn relayout(&mut self) {
        let sb = match self.scroll.scrollbar_size() {
            0 => app::scrollbar_size(),
            n => n,
        };
        // AUDIT FIX: a collapsed window gave a negative canvas width.
        let width = (self.scroll.w() - sb).max(1);
        let mut l = layout::layout(&self.page.items, &self.page.controls, width, &mut layout::FltkMeasure);
        l.viewport = (self.scroll.w(), self.scroll.h());
        let height = l.height.max(self.scroll.h());
        let ypos = self.scroll.yposition().min((height - self.scroll.h()).max(0));
        let (ox, oy) = (self.scroll.x(), self.scroll.y() - self.scroll.yposition());
        self.canvas.resize(ox, oy, width, height);
        for pc in &l.controls {
            if let Some(Some(w)) = self.widgets.get(pc.idx) {
                w.base().resize(ox + pc.x, oy + pc.y, pc.w, pc.h);
            }
        }
        *self.layout.borrow_mut() = l;
        self.scroll.scroll_to(0, ypos);
        self.scroll.redraw();
    }

    fn radio_toggled(&mut self, i: usize) {
        // AUDIT FIX: was `controls[i]` (panic on a stale index); see Msg.
        let Some((form, name)) = self.page.controls.get(i).map(|c| (c.form, c.name.clone())) else {
            return;
        };
        if name.is_empty() {
            return; // unnamed radios aren't grouped
        }
        for (j, spec) in self.page.controls.iter().enumerate() {
            if let (ControlKind::Radio { .. }, Some(Some(Ctl::Radio(w)))) = (&spec.kind, self.widgets.get(j)) {
                if spec.form == form && spec.name == name {
                    w.clone().set_value(j == i);
                }
            }
        }
    }

    /// HTML form submission: collect successful controls in tree order.
    fn form_submission(&self, form: usize, submitter: Option<usize>) -> Option<Request> {
        let spec = self.page.forms.get(form)?;
        if spec.dialog {
            return None; // method=dialog closes a <dialog>; nothing to send
        }
        // Implicit submission (Enter) uses the form's first submit button as submitter.
        let submitter = submitter.or_else(|| {
            self.page
                .controls
                .iter()
                .position(|c| c.form == Some(form) && matches!(c.kind, ControlKind::Submit { .. }))
        });
        let mut pairs = Vec::new();
        for (i, c) in self.page.controls.iter().enumerate() {
            if c.form != Some(form) || c.disabled {
                continue;
            }
            // AUDIT FIX: image buttons submit click coordinates as name.x/name.y (or x/y unnamed).
            if let ControlKind::Submit { image: true, .. } = c.kind {
                if submitter == Some(i) {
                    let prefix = if c.name.is_empty() { String::new() } else { format!("{}.", c.name) };
                    pairs.push((format!("{prefix}x"), "0".into()));
                    pairs.push((format!("{prefix}y"), "0".into()));
                }
                continue;
            }
            if c.name.is_empty() {
                continue;
            }
            let widget = self.widgets.get(i).and_then(Option::as_ref);
            // AUDIT FIX: multi-selects submit one pair per selected option.
            if let (ControlKind::Select { options, .. }, Some(Ctl::List(w))) = (&c.kind, widget) {
                for (k, o) in options.iter().enumerate() {
                    if i32::try_from(k + 1).is_ok_and(|line| w.selected(line)) && !o.disabled {
                        pairs.push((c.name.clone(), o.value.clone()));
                    }
                }
                continue;
            }
            let widget = self.widgets.get(i).and_then(Option::as_ref);
            let value = match (&c.kind, widget) {
                (ControlKind::Hidden { value }, _) => Some(value.clone()),
                (ControlKind::Text { .. }, Some(Ctl::Text(w))) => Some(w.value()),
                (ControlKind::Text { .. }, Some(Ctl::Secret(w))) => Some(w.value()),
                (ControlKind::TextArea { .. }, Some(Ctl::Area(w))) => {
                    Some(w.value().replace("\r\n", "\n").replace('\n', "\r\n"))
                }
                (ControlKind::Checkbox { value, .. }, Some(Ctl::Check(w))) => w.value().then(|| value.clone()),
                (ControlKind::Radio { value, .. }, Some(Ctl::Radio(w))) => w.value().then(|| value.clone()),
                (ControlKind::Select { options, .. }, Some(Ctl::Choice(w))) => usize::try_from(w.value())
                    .ok()
                    .and_then(|k| options.get(k))
                    .filter(|o| !o.disabled)
                    .map(|o| o.value.clone()),
                (ControlKind::Submit { value, .. }, _) if submitter == Some(i) => Some(value.clone()),
                _ => None,
            };
            if let Some(v) = value {
                pairs.push((c.name.clone(), v));
            }
        }
        // AUDIT FIX: honour the submitter's formaction/formmethod/formenctype.
        let mut spec = spec.clone();
        if let Some(ControlKind::Submit { overrides, .. }) = submitter.and_then(|i| self.page.controls.get(i)).map(|c| &c.kind) {
            if let Some(a) = &overrides.action {
                spec.action = a.clone();
            }
            spec.post = overrides.post.unwrap_or(spec.post);
            spec.multipart = overrides.multipart.unwrap_or(spec.multipart);
        }
        let referer = self.current.as_ref().map(|r| r.url.clone());
        Some(net::form_request(&spec, &pairs, referer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omnibox() {
        let u = |s: &str| omnibox_url(s).map(|u| u.to_string());
        assert_eq!(u("example.com/x").as_deref(), Some("https://example.com/x"));
        assert_eq!(u("http://a.b/").as_deref(), Some("http://a.b/"));
        assert_eq!(u("localhost:8080").as_deref(), Some("https://localhost:8080/"));
        assert_eq!(u("rust fltk").as_deref(), Some("https://lite.duckduckgo.com/lite/?q=rust+fltk"));
        assert_eq!(u("   "), None);
    }

    #[test]
    fn labels_escape_at_signs() {
        assert_eq!(label_escape("https://x.com/@user"), "https://x.com/@@user");
    }
}
