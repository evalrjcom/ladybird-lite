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
| UI                 | FLTK Window + Input + Button + TextDisplay |
| RequestServer      | reqwest blocking client (timeout, UA)   |
| HTML Parser        | html5ever + markup5ever_rcdom           |
| CSS Engine         | lightningcss (parse + rule count)       |
| JS Engine          | quickjs-ng via quickjs-rusty            |
| Rendering          | Extracted text content (basic layout)   |
| Process isolation  | Single process (future: spawn helpers)  |

### What Works

- Address bar + Go button (Enter also works)
- Fetch any HTTP/HTTPS URL with timeout and User-Agent
- Parse HTML5 into a DOM
- Extract `<title>`, visible text, `<script>`, and `<style>`
- Parse CSS with lightningcss
- Execute `<script>` content in QuickJS
- Host bindings: `alert()` and `consoleLog()`
- Dark themed FLTK UI

### Limitations (intentional for a from-scratch prototype)

- No full CSS layout/painting (text extraction only)
- No DOM mutation from JS beyond host callbacks
- No images, forms, or advanced web APIs
- Single process
- Basic error handling

## Building & Running

### Prerequisites

- Rust 1.70+ (stable)
- On Linux: development packages for FLTK (usually handled by fltk-bundled)
- On Windows: MSVC or MinGW toolchain

```bash
cargo run --release
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
