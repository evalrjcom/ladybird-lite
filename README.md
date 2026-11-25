# Mini Ladybird

A from-scratch, cross-platform (Windows + Linux) minimal browser prototype inspired by the **Ladybird** architecture.

Built with:
- **Rust** for safety and performance
- **FLTK** for a lightweight native UI
- **reqwest** (rustls) for modern HTTP/1.1 + HTTP/2
- **html5ever** (Servo) for standards-compliant HTML parsing
- **lightningcss** for CSS parsing
- **quickjs-ng** (via quickjs-rusty) for JavaScript

This is **not** a full browser. It demonstrates the core pipeline of a Ladybird-style engine in a single process for practicality, while remaining modular enough to later split into sandboxed processes.

## Ladybird Architecture Outline

Ladybird uses a multi-process, aggressively sandboxed design:

```
                             UI
                              |
                             Hub
                              |
     +----------+-------------+-------+----------------+
     |          |                     |                |
 Compositor  WebContent         RequestServer    ImageDecoder
             (renderer)         (per site)       (per site)
                 |
     +-----------+-----------+
     |           |           |
 WebRTCClient  MediaServer  AudioServer
```

### Process Roles

| Process       | Role                                      | Isolation |
|---------------|-------------------------------------------|-----------|
| **UI**        | Tabs, address bar, menus, user input      | Privileged |
| **Hub**       | Process launcher, shared state (cookies, storage, fonts) | Coordinator |
| **WebContent**| LibWeb (HTML/CSS/DOM/layout/paint) + LibJS | Sandboxed per site |
| **RequestServer** | HTTP/HTTPS, DNS, cookies               | Sandboxed |
| **ImageDecoder** | Image decoding (PNG, JPEG, WebP, etc.) | Sandboxed |
| **Compositor**| Frame composition, scrolling, presentation | Sandboxed |
| Others        | Audio, Media, WasmCompiler, WebWorkers    | Sandboxed |

### Core Libraries (from SerenityOS lineage)

- **LibWeb**: HTML parser → DOM → CSS cascade (StyleComputer) → layout (box tree) → paint (display list)
- **LibJS**: Spec-compliant JS engine with bytecode interpreter and GC
- **LibHTTP / LibTLS / LibCrypto**: Networking and security
- **LibGfx**: 2D graphics, fonts, image handling
- **LibIPC**: Typed inter-process communication
- **LibCore**: Event loop and OS abstraction

### Loading-to-Painting Pipeline

1. UI sends navigate request
2. RequestServer fetches the resource (HTTP/2, TLS)
3. HTML parser builds the DOM tree
4. StyleComputer computes cascaded styles
5. Layout constructs the box tree
6. Paint generates a paint tree / display list
7. Compositor rasterizes and presents
8. Scripts run in LibJS and can mutate the DOM, triggering re-layout

All of this is implemented from web standards (no Blink/WebKit/Gecko code).

## This Prototype's Mapping

| Ladybird Component | This Prototype                          |
|--------------------|-----------------------------------------|
| UI                 | FLTK Window + Flex (Back, address bar, Go) + Scroll view |
| RequestServer      | `src/net.rs`: reqwest blocking client (timeout, cookies, browser UA, GET/POST) |
| HTML Parser        | `src/page.rs`: html5ever + markup5ever_rcdom → inline flow of words, links and form controls |
| Layout + Paint     | `src/layout.rs`: word-wrapping inline layout, re-run on resize; text painted with `fltk::draw` |
| Forms              | Real FLTK widgets placed in the flow; HTML form submission (urlencoded GET/POST) |
| CSS Engine         | lightningcss (parse + rule count)       |
| JS Engine          | quickjs-ng via quickjs-rusty            |
| Process isolation  | Single process (future: spawn helpers)  |

### What Works

- Address bar + Go button (Enter also works), Back button, optional start URL: `mini-ladybird <url>`
- Fetch any HTTP/HTTPS URL with connect/overall timeouts, cookies, up to 10 redirects, gzip/brotli
  and a browser-like User-Agent. Bodies are streamed with an **8 MiB cap** (the page is marked
  truncated). Text is decoded using the `Content-Type` charset, a `<meta charset>` prescan, BOM or
  UTF-8 (Latin-1, Shift_JIS, … display correctly)
- Content routing: HTML/XHTML is rendered, `text/*` and JSON are shown as plain text, other types
  (images, binaries) show a "can't display" notice instead of garbage
- **Omnibox**: typing words (not a URL) in the address bar searches DuckDuckGo Lite; `example.com`
  gets `https://` added
- Parse HTML5 into a DOM; extract `<title>` (shown in the window title), `<script>`, `<style>`
- **Word wrap**: text flows and wraps to the window width and re-wraps on resize; no horizontal
  scrolling (over-long tokens such as URLs are broken by characters). Headings, bold, monospace,
  lists, tables (rows → lines), `<pre>`, `<hr>`, `<br>` and image alt text are rendered.
- **Clickable links**: `<a href>` is resolved against the page URL (and the first `<base href>`);
  click to navigate (hand cursor on hover). `#fragment` links scroll to the matching `id` /
  `<a name>`. The Referer follows `strict-origin-when-cross-origin` (no fragment/credentials,
  origin only cross-site, nothing on https→http).
- **Web forms** rendered as real FLTK widgets in document order:
  `<input type=text|search|email|url|tel|number|… >`, `password`, `hidden`, `checkbox`,
  `radio` (exclusive per form + name), `submit`/`image`, `<button>`, `<textarea>`,
  `<select>`/`<option>`/`<optgroup>`, `<select multiple>` (list box); `disabled`, `readonly`,
  `maxlength`, `autofocus`, `<fieldset disabled>` and the `form=` attribute are honoured
- **Form submission** per the HTML algorithm: successful controls in tree order, `name=value`
  urlencoded (`application/x-www-form-urlencoded`) or `multipart/form-data`; the submitter's
  `formaction`/`formmethod`/`formenctype` override the form; image buttons send `name.x/name.y`; `method=GET` (query string replaces the
  action's query) or `POST` (request body), `action` resolved against the page URL (empty → current
  page). After a POST, **Back asks before re-sending the form data**; a POST→303 redirect result is a
  normal GET page. Pressing **Enter in a text field submits its form** (implicit submission uses the first
  submit button as submitter). Example: open `https://duckduckgo.com/lite/`, type a query, press Enter.
- **Keyboard scrolling that works whatever has focus**: PageDown/PageUp scroll one screen (minus a
  40px overlap) even while typing in the search box or the URL bar; Home/End, ↑/↓ and
  Space/Shift+Space scroll when focus isn't in a text field (click the page text to leave a field).
  A `<textarea>` keeps PageUp/PageDown only when its own text overflows. The mouse wheel scrolls
  ~3 lines per notch. Logic and unit tests live in `src/scrolling.rs`.
- Parse CSS with lightningcss; execute inline classic `<script>` in QuickJS (`alert()`, `consoleLog()`)
  in a **fresh context per page** with a 32 MB heap limit and a 1.5 s time budget (infinite loops are
  interrupted), and at most 3 alerts per page
- Hostile-page hardening: deep nesting (100k+ elements), megabyte-long words and huge pages load in
  well under 2 s with bounded memory; `@` in titles/labels is shown literally
- Dark themed FLTK UI

### Limitations (intentional for a from-scratch prototype)

- CSS is parsed but **not applied** (no colours, `display:none`, positioning, floats or columns), so
  menus and elements hidden by CSS are shown, and layout is a single flowing column
- JavaScript has no DOM: pages that build their UI with JS will not work; JS-only form handlers
  (`onsubmit`, `fetch`) are ignored
- No images (alt text only), no file upload (`<input type=file>`), no `<input type=reset>` behaviour
- Back re-requests the previous page (no page cache)
- Networking is blocking on the UI thread; single process
- Basic error handling

## Building & Running

### Prerequisites

- A recent stable Rust (tested with 1.100; 1.85 is too old for the current dependencies)
- On Linux: X11/Xft/Pango/Cairo development packages (FLTK is bundled, but links against them), e.g.
  `sudo apt-get install libx11-dev libxext-dev libxft-dev libxinerama-dev libxcursor-dev libxrender-dev libxfixes-dev libfontconfig1-dev libpango1.0-dev libcairo2-dev libgl1-mesa-dev libglu1-mesa-dev`
  and `libclang-dev` (bindgen for QuickJS)
- On Windows: MSVC or MinGW toolchain

```bash
cargo run --release                                  # opens with the DuckDuckGo Lite URL prefilled
cargo run --release -- https://duckduckgo.com/lite/  # load a page at startup
cargo test                                           # parser, layout, net, form and scrolling tests
```

The binary is self-contained thanks to `fltk-bundled`.

### Cross-compilation notes

- Linux → Windows: use `cargo-xwin` or cross
- The `fltk-bundled` feature statically links FLTK, simplifying distribution

## Future Extensions (to better match Ladybird)

1. Split into processes with IPC (use `ipc-channel` or custom sockets)
2. Implement a real layout engine (or integrate Servo layout)
3. Canvas-based painting with computed styles
4. Full DOM API bindings for QuickJS
5. Site isolation and sandboxing (`seccomp`, AppContainer, etc.)
6. HTTP/3 via quinn

## License

MIT
