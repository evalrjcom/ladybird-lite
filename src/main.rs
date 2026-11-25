//! Mini Ladybird — a from-scratch cross-platform browser prototype
//! Inspired by Ladybird's multi-process, standards-first architecture.
//!
//! Architecture mapping (single-process for the prototype; modular for future isolation):
//! - UI layer: FLTK (address bar, navigation, scrollable content view)
//! - Network (`net`): reqwest (HTTP/1.1 + HTTP/2, rustls, cookies, compression)
//! - HTML (`page`): html5ever (WHATWG parser) → RcDom → inline flow of words + form controls
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

use anyhow::{Context, Result};
use fltk::{
    app,
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
use url::Url;

#[derive(Debug, Clone)]
enum Msg {
    /// Navigate to whatever is in the address bar.
    Go,
    Back,
    /// Follow link `idx` of the current page.
    Link(usize),
    /// Submit form `form`, optionally via the submit button control `submitter`.
    Submit { form: usize, submitter: Option<usize> },
    /// A radio button was toggled: enforce exclusivity within its group.
    Radio(usize),
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
}

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
    col.end();
    win.end();
    win.resizable(&col);
    win.make_resizable(true);
    win.show();
    col.layout();

    // Text layer: draws the laid-out words; clicks on links navigate.
    let layout = Rc::new(RefCell::new(Layout::default()));
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
            layout::paint(&l, c.x(), c.y(), c.w(), c.h());
        });
    }
    {
        let l = layout.clone();
        let s = sender;
        canvas.handle(move |c, ev| {
            let hit = || layout::link_at(&l.borrow(), app::event_x() - c.x(), app::event_y() - c.y());
            match ev {
                Event::Enter => true,
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
                Event::Push => hit().is_some(),
                Event::Released => {
                    if let Some(i) = hit() {
                        s.send(Msg::Link(i));
                    }
                    true
                }
                _ => false,
            }
        });
    }

    back_btn.emit(sender, Msg::Back);
    go_btn.emit(sender, Msg::Go);
    url_input.emit(sender, Msg::Go);
    win.resize_callback(move |_, _, _, _, _| sender.send(Msg::Relayout));

    let js_ctx = JsContext::builder().build().context("Failed to create QuickJS context")?;
    js_ctx.add_callback("alert", |msg: String| {
        fltk::dialog::alert_default(&msg);
        true
    })?;
    js_ctx.add_callback("consoleLog", |msg: String| {
        eprintln!("[JS] {}", msg);
        true
    })?;

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

impl Browser {
    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Go => {
                let raw = self.url_input.value();
                let raw = raw.trim();
                let fixed = if raw.contains("://") { raw.to_string() } else { format!("https://{raw}") };
                match Url::parse(&fixed) {
                    Ok(u) => self.load(Request::get(u), true),
                    Err(e) => self.status.set_label(&format!("Invalid URL: {e}")),
                }
            }
            Msg::Back => {
                if let Some(prev) = self.history.pop() {
                    self.load(prev, false);
                }
            }
            Msg::Link(i) => {
                if let Some(u) = self.page.links.get(i).cloned() {
                    if self.is_same_document_fragment(&u) {
                        self.status.set_label(&format!("(in-page anchor) {u}"));
                        return;
                    }
                    let referer = self.current.as_ref().map(|r| r.url.clone());
                    self.load(Request { url: u, form_body: None, referer }, true);
                }
            }
            Msg::Submit { form, submitter } => {
                if let Some(req) = self.form_submission(form, submitter) {
                    self.load(req, true);
                }
            }
            Msg::Radio(i) => self.radio_toggled(i),
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

    fn load(&mut self, req: Request, push_history: bool) {
        if !matches!(req.url.scheme(), "http" | "https") {
            self.status.set_label(&format!("Unsupported scheme: {}", req.url.scheme()));
            return;
        }
        let verb = if req.form_body.is_some() { "POST" } else { "GET" };
        self.status.set_label(&format!("{verb} {} …", req.url));
        self.win.set_cursor(Cursor::Wait);
        app::flush();

        let result = net::fetch(&self.client, &req);
        self.win.set_cursor(Cursor::Default);
        match result {
            Ok(resp) => {
                let page = page::parse(&resp.body, &resp.url)
                    .unwrap_or_else(|e| Page::message("Parse error", &e.to_string()));
                self.url_input.set_value(resp.url.as_str());
                if push_history {
                    if let Some(cur) = self.current.take() {
                        self.history.push(cur);
                    }
                }
                // Keep the method/body so Back can re-show POST results (e.g. DDG lite).
                self.current = Some(Request { url: resp.url.clone(), ..req });
                self.page = page;
                self.run_page_resources();
                self.show_page();
                self.status.set_label(&format!(
                    "{verb} {} {} — {} links, {} form(s)",
                    resp.status,
                    resp.url,
                    self.page.links.len(),
                    self.page.forms.len()
                ));
            }
            Err(e) => {
                self.page = Page::message("Navigation failed", &format!("{}\n{:#}", req.url, e));
                self.show_page();
                self.status.set_label(&format!("Error: {e:#}"));
            }
        }
    }

    fn run_page_resources(&mut self) {
        for css in &self.page.styles {
            match StyleSheet::parse(css, ParserOptions::default()) {
                Ok(sheet) => log::info!("Parsed CSS stylesheet with {} rules", sheet.rules.0.len()),
                Err(e) => log::warn!("CSS parse error: {:?}", e),
            }
        }
        for script in &self.page.scripts {
            if let Err(e) = self.js_ctx.eval(script, false) {
                log::warn!("JS error: {}", e);
            }
        }
    }

    /// Replace form widgets for the new page and lay everything out.
    fn show_page(&mut self) {
        for w in self.widgets.drain(..).flatten() {
            let base = w.base();
            self.scroll.remove(&base);
            app::delete_widget(base);
        }
        let title = self.page.title.clone().unwrap_or_else(|| "(untitled)".into());
        self.win.set_label(&format!("{title} — Mini Ladybird"));

        self.scroll.begin();
        let s = self.sender;
        for (i, spec) in self.page.controls.iter().enumerate() {
            let form = spec.form;
            let submit = move |submitter: Option<usize>| form.map(|form| Msg::Submit { form, submitter });
            let ctl = match &spec.kind {
                ControlKind::Hidden { .. } => None,
                ControlKind::Text { value, password, .. } => {
                    // Enter in a text field submits its form (implicit submission).
                    let msg = submit(None);
                    if *password {
                        let mut w = SecretInput::default();
                        w.set_value(value);
                        w.set_trigger(CallbackTrigger::EnterKeyAlways);
                        if let Some(m) = msg {
                            w.set_callback(move |_| s.send(m.clone()));
                        }
                        Some(Ctl::Secret(w))
                    } else {
                        let mut w = Input::default();
                        w.set_value(value);
                        w.set_trigger(CallbackTrigger::EnterKeyAlways);
                        if let Some(m) = msg {
                            w.set_callback(move |_| s.send(m.clone()));
                        }
                        Some(Ctl::Text(w))
                    }
                }
                ControlKind::TextArea { value, .. } => {
                    let mut w = MultilineInput::default();
                    w.set_value(value);
                    w.set_wrap(true);
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
                    w.set_callback(move |_| s.send(Msg::Radio(i)));
                    Some(Ctl::Radio(w))
                }
                ControlKind::Submit { label, .. } => {
                    let mut w = Button::default().with_label(&label.replace('@', "@@"));
                    if let Some(m) = submit(Some(i)) {
                        w.set_callback(move |_| s.send(m.clone()));
                    }
                    Some(Ctl::Button(w))
                }
                ControlKind::Button { label } => {
                    let mut w = Button::default().with_label(&label.replace('@', "@@"));
                    w.deactivate();
                    Some(Ctl::Button(w))
                }
                ControlKind::Select { options, selected } => {
                    let mut w = Choice::default();
                    for (label, _) in options {
                        let esc = label
                            .replace('\\', "\\\\")
                            .replace('/', "\\/")
                            .replace('&', "&&")
                            .replace('|', " ");
                        w.add_choice(if esc.is_empty() { " " } else { &esc });
                    }
                    w.set_value(*selected as i32);
                    Some(Ctl::Choice(w))
                }
            };
            if let Some(c) = &ctl {
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
        // Focus the first text field (like DDG's autofocus).
        if let Some(Some(Ctl::Text(w))) = self.widgets.iter().find(|w| matches!(w, Some(Ctl::Text(_)))) {
            let _ = w.clone().take_focus();
        }
    }

    /// Lay out for the current viewport width and position canvas + widgets.
    fn relayout(&mut self) {
        let sb = match self.scroll.scrollbar_size() {
            0 => app::scrollbar_size(),
            n => n,
        };
        let width = self.scroll.w() - sb;
        let mut l = layout::layout(&self.page.items, &self.page.controls, width);
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
        let (form, name) = {
            let c = &self.page.controls[i];
            (c.form, c.name.clone())
        };
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
        // Implicit submission (Enter) uses the form's first submit button as submitter.
        let submitter = submitter.or_else(|| {
            self.page
                .controls
                .iter()
                .position(|c| c.form == Some(form) && matches!(c.kind, ControlKind::Submit { .. }))
        });
        let mut pairs = Vec::new();
        for (i, c) in self.page.controls.iter().enumerate() {
            if c.form != Some(form) || c.disabled || c.name.is_empty() {
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
                    .map(|(_, v)| v.clone()),
                (ControlKind::Submit { value, .. }, _) if submitter == Some(i) => Some(value.clone()),
                _ => None,
            };
            if let Some(v) = value {
                pairs.push((c.name.clone(), v));
            }
        }
        let referer = self.current.as_ref().map(|r| r.url.clone());
        Some(net::form_request(spec, &pairs, referer))
    }
}
