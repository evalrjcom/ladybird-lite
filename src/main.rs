//! Mini Ladybird — a from-scratch cross-platform browser prototype
//! Inspired by Ladybird's multi-process, standards-first architecture.
//!
//! Architecture mapping (single-process for the prototype; modular for future isolation):
//! - UI layer: FLTK (address bar, navigation, content view)
//! - Network: reqwest (modern HTTP/1.1 + HTTP/2, rustls, compression)
//! - HTML: html5ever (Servo/WHATWG-grade parser) → RcDom
//! - CSS: lightningcss (typed, browser-grade parser)
//! - JavaScript: quickjs-ng via quickjs-rusty
//!
//! Security notes:
//! - Network calls have timeouts and a fixed User-Agent
//! - HTML/CSS/JS parsing is bounded where practical
//! - No arbitrary file access from page content
//! - Scripts run in an embedded engine with limited host bindings

use anyhow::{Context, Result};
use fltk::{
    app,
    button::Button,
    enums::{Align, Color, Font, FrameType},
    frame::Frame,
    input::Input,
    prelude::*,
    text::{TextBuffer, TextDisplay},
    window::Window,
};
use html5ever::{parse_document, tendril::TendrilSink, ParseOpts};
use lightningcss::{
    stylesheet::{ParserOptions, StyleSheet},
};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use quickjs_rusty::Context as JsContext;
use reqwest::blocking::Client;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use url::Url;

const USER_AGENT: &str = "MiniLadybird/0.1 (Rust+FLTK; +https://ladybird.org)";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

struct BrowserState {
    url_input: Input,
    content: TextDisplay,
    status: Frame,
    js_ctx: JsContext,
}

fn main() -> Result<()> {
    env_logger::init();
    let app = app::App::default().with_scheme(app::Scheme::Gtk);

    let mut win = Window::new(100, 100, 900, 640, "Mini Ladybird — Rust + FLTK");
    win.set_color(Color::from_rgb(30, 30, 30));

    let mut url_input = Input::new(10, 10, 760, 30, "");
    url_input.set_value("https://example.com");
    url_input.set_color(Color::from_rgb(50, 50, 50));
    url_input.set_text_color(Color::White);
    url_input.set_text_font(Font::Courier);

    let mut go_btn = Button::new(780, 10, 110, 30, "Go");
    go_btn.set_color(Color::from_rgb(70, 130, 180));
    go_btn.set_label_color(Color::White);

    let mut status = Frame::new(10, 50, 880, 20, "Ready");
    status.set_label_color(Color::from_rgb(180, 180, 180));
    status.set_align(Align::Left | Align::Inside);
    status.set_frame(FrameType::FlatBox);
    status.set_color(Color::from_rgb(40, 40, 40));

    let mut buffer = TextBuffer::default();
    let mut content = TextDisplay::new(10, 80, 880, 540, "");
    content.set_buffer(buffer.clone());
    content.set_color(Color::from_rgb(25, 25, 25));
    content.set_text_color(Color::from_rgb(220, 220, 220));
    content.set_text_font(Font::Courier);
    content.set_text_size(14);
    content.set_linenumber_width(40);

    win.end();
    win.show();

    let js_ctx = JsContext::new().context("Failed to create QuickJS context")?;
    // Basic host bindings — closures must return a value to implement Callback
    js_ctx.add_callback("alert", |msg: String| {
        fltk::dialog::alert_default(&msg);
        ()
    })?;
    js_ctx.add_callback("consoleLog", |msg: String| {
        eprintln!("[JS] {}", msg);
        ()
    })?;

    let state = Rc::new(RefCell::new(BrowserState {
        url_input,
        content,
        status,
        js_ctx,
    }));

    let state_clone = state.clone();
    go_btn.set_callback(move |_| {
        let url = state_clone.borrow().url_input.value();
        if let Err(e) = navigate(&state_clone, &url) {
            let mut st = state_clone.borrow_mut();
            st.status.set_label(&format!("Error: {}", e));
            st.content.buffer().unwrap().set_text(&format!("Navigation failed:\n{}", e));
        }
    });

    // Enter key in input also navigates
    let state_enter = state.clone();
    url_input.set_callback(move |_| {
        let url = state_enter.borrow().url_input.value();
        if let Err(e) = navigate(&state_enter, &url) {
            let mut st = state_enter.borrow_mut();
            st.status.set_label(&format!("Error: {}", e));
        }
    });

    app.run()?;
    Ok(())
}

fn navigate(state: &Rc<RefCell<BrowserState>>, raw_url: &str) -> Result<()> {
    let mut st = state.borrow_mut();
    st.status.set_label("Fetching…");
    app::awake(); // force UI update

    let url = Url::parse(raw_url).context("Invalid URL")?;
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .build()?;

    let response = client.get(url.as_str()).send().context("Request failed")?;
    let status_code = response.status();
    let body = response.text().context("Failed to read body")?;

    st.status.set_label(&format!("Parsing {} ({} bytes)", status_code, body.len()));
    app::awake();

    let (title, text, scripts, styles) = parse_html(&body)?;

    // Apply CSS (basic: just log or extract rules; full cascade is future work)
    if !styles.is_empty() {
        for css in &styles {
            match StyleSheet::parse(css, ParserOptions::default()) {
                Ok(sheet) => {
                    log::info!("Parsed CSS stylesheet with {} rules", sheet.rules.0.len());
                    // Future: apply computed styles to nodes
                }
                Err(e) => log::warn!("CSS parse error: {:?}", e),
            }
        }
    }

    // Run scripts — eval takes (code, resolve_modules)
    for script in scripts {
        if let Err(e) = st.js_ctx.eval(&script, false) {
            log::warn!("JS error: {}", e);
            st.content.buffer().unwrap().append(&format!("\n[JS Error] {}\n", e));
        }
    }

    let display = if text.is_empty() {
        "(empty document)".to_string()
    } else {
        format!("Title: {}\n\n{}", title.unwrap_or_else(|| "(no title)".into()), text)
    };

    st.content.buffer().unwrap().set_text(&display);
    st.status.set_label(&format!("Loaded {} — {} chars", url.host_str().unwrap_or(""), display.len()));
    Ok(())
}

fn parse_html(html: &str) -> Result<(Option<String>, String, Vec<String>, Vec<String>)> {
    let opts = ParseOpts::default();
    let dom = parse_document(RcDom::default(), opts)
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .context("HTML parse failed")?;

    let mut title = None;
    let mut text = String::new();
    let mut scripts = Vec::new();
    let mut styles = Vec::new();

    fn walk(
        handle: &Handle,
        title: &mut Option<String>,
        text: &mut String,
        scripts: &mut Vec<String>,
        styles: &mut Vec<String>,
        in_title: bool,
        in_script: bool,
        in_style: bool,
    ) {
        let node = handle;
        match &node.data {
            NodeData::Element { name, .. } => {
                let local: &str = name.local.as_ref();
                let is_title = local.eq_ignore_ascii_case("title");
                let is_script = local.eq_ignore_ascii_case("script");
                let is_style = local.eq_ignore_ascii_case("style");
                let is_br = local.eq_ignore_ascii_case("br") || local.eq_ignore_ascii_case("p") || local.eq_ignore_ascii_case("div");

                if is_br && !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }

                for child in node.children.borrow().iter() {
                    walk(
                        child,
                        title,
                        text,
                        scripts,
                        styles,
                        in_title || is_title,
                        in_script || is_script,
                        in_style || is_style,
                    );
                }
            }
            NodeData::Text { contents } => {
                let s = contents.borrow().to_string();
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    if in_title {
                        *title = Some(trimmed.to_string());
                    } else if in_script {
                        scripts.push(trimmed.to_string());
                    } else if in_style {
                        styles.push(trimmed.to_string());
                    } else {
                        if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                            text.push(' ');
                        }
                        text.push_str(trimmed);
                    }
                }
            }
            _ => {
                for child in node.children.borrow().iter() {
                    walk(child, title, text, scripts, styles, in_title, in_script, in_style);
                }
            }
        }
    }

    walk(&dom.document, &mut title, &mut text, &mut scripts, &mut styles, false, false, false);
    Ok((title, text, scripts, styles))
}
