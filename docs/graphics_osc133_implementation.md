# mypixels `rust_rewrite`: image graphics + OSC 133 implementation plan

**Target:** [`volca02/mypixels`, branch `rust_rewrite`](https://github.com/volca02/mypixels/tree/rust_rewrite)  
**Inspected:** 2026-11-03  
**Status:** M0-M2 (OSC 133) and M3-M5 (Kitty graphics) are IMPLEMENTED. The
implementation deviated in one important way from the M3-M5 design below: the
Kitty parameter language, transfer assembly, image store and placement rules
live in the application (`src/kitty.rs`, pure and unit-tested), not inside
vendored alacritty_terminal. The vendored terminal does only what it uniquely
can - notice the sequence and report the cursor's absolute position at that
instant (`osc_raw`/`apc_raw` hooks in `vendor/vte`, `Event::Semantic` /
`Event::Apc` in `vendor/alacritty_terminal`) and hand over bytes. M6 and the
sixel/iTerm2 milestones remain design; code fragments there are still
pseudocode.

## Objective

Add:

1. **OSC 133 shell semantic marks**: prompt start/end, command start/end, command exit code, per-tab command navigation.
2. **Kitty Graphics Protocol**: PNG uploads, chunked transfers, image IDs, placements, delete, scroll/resize behavior, wgpu textured rendering.
3. **Sixel and iTerm2 inline images** in a subsequent milestone, using the same image store and renderer.

Keep the existing `alacritty_terminal` PTY/event loop, `glyphon` text rendering, tab/window behavior, and BEL-based failure notification.

## Confirmed repository anatomy

Files and behaviors inspected directly on `rust_rewrite`:

| Path | Current role | New responsibility |
|---|---|---|
| [`Cargo.toml`](https://github.com/volca02/mypixels/blob/rust_rewrite/Cargo.toml) | `alacritty_terminal = "0.26.0"`, `wgpu = "30"`, `glyphon = "0.12.0"`, `winit = "0.30"`, `portable-pty = "0.9"`; patches `vendor/cosmic-text` | Add image dependencies and path patch for Alacritty |
| [`src/tab.rs`](https://github.com/volca02/mypixels/blob/rust_rewrite/src/tab.rs) | `Tab` owns `Arc<FairMutex<Term<SpikeListener>>>`, PTY sender and event-loop thread; `SpikeListener` handles `Event::Bell`, `Event::Wakeup`, title, exit | Per-tab graphics/semantic stores; event/operation synchronization |
| [`src/main.rs`](https://github.com/volca02/mypixels/blob/rust_rewrite/src/main.rs) | `App::build_frame()` snapshots cells through `term.renderable_content()`; `App::draw()` builds rectangles and glyphon text areas | Collect visible image placements; draw command decorations |
| [`src/gpu.rs`](https://github.com/volca02/mypixels/blob/rust_rewrite/src/gpu.rs) | `Gpu::render_frame()` renders rectangles, then glyphon, then egui in one pass | Add textured-quad pipeline and image batches between appropriate layers |
| [`scripts/shell_integration.bash`](https://github.com/volca02/mypixels/blob/rust_rewrite/scripts/shell_integration.bash) | `PROMPT_COMMAND` rings BEL on failed command | Add safe OSC 133 markers without losing BEL |
| [`scripts/shell_integration.zsh`](https://github.com/volca02/mypixels/blob/rust_rewrite/scripts/shell_integration.zsh) | `precmd` BEL on failed command | Add `preexec` + OSC 133 |
| [`scripts/shell_integration.fish`](https://github.com/volca02/mypixels/blob/rust_rewrite/scripts/shell_integration.fish) | `fish_postexec` BEL on failed command | Add fish prompt/execute hooks + OSC 133 |
| [`tests/`](https://github.com/volca02/mypixels/tree/rust_rewrite/tests) | PTY/UI smoke tests | Add protocol test vectors and image smoke tests |

**Important existing decisions:**

- `Tab::new()` calls `Term::new(...)`, `tty::setup_env()`, then `tty::new(...)` and `EventLoop::new(...).spawn()`. **Do not insert a second unsynchronized PTY reader.**
- The app currently overrides `TERM` to `xterm-256color` by default. Do not treat `TERM` as an adequate graphics capability signal; consider explicit feature negotiation and configuration.
- The existing `src/gpu.rs` frame pass draws **rectangles → glyphon text → egui**. A single graphics layer before glyphon is a practical initial implementation, but proper Kitty z-order eventually requires multiple image batches.
- The project already has `png = "0.17"` for other functionality. Retain it unless a decoder-specific need justifies another crate.
- `src/main.rs` uses a *snapshot frame* (`FrameData`) created while holding the terminal lock. Extend this pattern; avoid decoding or uploading textures under that lock.
- Current failure notification is **BEL**. OSC 133 is complementary; keep BEL behavior and prevent double notifications.

## Milestones and acceptance gates

- [ ] M0 Baseline and dependency pinning
- [ ] M1 OSC 133 parsing and metadata
- [ ] M2 Shell integration and visible command navigation
- [x] M3 Kitty parsing + bounded payload assembly (2026-11-03: `src/kitty.rs`;
      PNG/f=100 only, `a=t/T/p/d`, all three continuation shapes, caps at
      32 MB transfer / 8192 px side / 64 images / 1024 placements; raw RGB,
      zlib and file transfer explicitly rejected, not silently mispainted)
- [x] M4 Kitty image store + scrolling semantics (placements anchored to the
      absolute logical line captured at dispatch; scroll with content and
      clip at the viewport edge by UV-splitting the quad; `a=d` deletes by
      image, placement, or all; textures and store die with the tab)
- [x] M5 wgpu image rendering (textured-quad pipeline in `src/gpu.rs`:
      Rgba8Unorm upload + linear sampler, painted after rects and before
      glyphs so text stays readable on top; texture cache keyed by
      (tab, id, generation))
- [ ] M6 Full Kitty operations and capability detection (deferred deliberately:
      `a=q` is IGNORED, not answered falsely; `kitty +kitten icat` therefore
      may not detect us - stream-mode senders work. Also open: scroll-cursor
      placement modes, alt-screen clear, images in switcher thumbnails,
      copy-image-to-clipboard from the stored source bytes)
- [ ] M7 Sixel and iTerm2 adapters
- [ ] M8 Robustness, performance, and release validation

Every milestone should preserve `cargo test`, `cargo clippy --all-targets`, and the existing UI/PTY smoke tests.

---

# M0 — Establish the baseline

## Step 1. Work on a dedicated branch

```bash
git clone --branch rust_rewrite https://github.com/volca02/mypixels.git
cd mypixels
git switch -c feature/graphics-osc133
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo run --release
```

Record exact versions from `Cargo.lock` and the tested commit:

```bash
git rev-parse HEAD
cargo tree -i alacritty_terminal
cargo tree -i wgpu
```

**Exit criterion:** existing terminal opens, types, scrolls, switches tabs, and notifies on failed commands.

## Step 2. Vendor `alacritty_terminal` at exactly the locked revision

This project currently consumes `alacritty_terminal` from crates.io, not a fork. Since the official event loop owns its VTE parser, graphics and OSC 133 must be dispatched *inside the same terminal state machine*. Do **not** strip escape bytes in a separate PTY thread: it could race with cursor motion, scrolls, RIS/alternate-screen transitions, and parser chunk boundaries.

One reproducible vendor workflow:

```bash
cargo vendor vendor/cargo
# Alternative: copy the exact alacritty_terminal-0.26.0 crate from
# ~/.cargo/registry/src/.../alacritty_terminal-0.26.0
# into vendor/alacritty_terminal, retaining upstream license files.
```

Then update the existing `Cargo.toml` (retain the existing `cosmic-text` patch):

```toml
[dependencies]
alacritty_terminal = "=0.26.0"
base64 = "0.22"
flate2 = "1"
# existing wgpu, png, glyphon, and other dependencies remain

[patch.crates-io]
cosmic-text = { path = "vendor/cosmic-text" }
alacritty_terminal = { path = "vendor/alacritty_terminal" }
```

**Note:** `cargo vendor vendor/cargo` is a convenience for downloading/checking dependency sources; it does *not* automatically create `vendor/alacritty_terminal` at the required path. Copy the exact crate explicitly or set the patch path to the actual vendor location. Also verify the copied crate's own `Cargo.toml` version/name and its compatibility with `alacritty_terminal` dependencies. A Git checkout at `master` is **not** a substitute for a locked 0.26.0 source.

Prefer one small, documented patch series over wholesale cherry-picks from unrelated forks. Save the initial vendored source baseline before edits.

## Step 3. Define capability flags

Expose opt-in settings first:

```yaml
terminal:
  osc133: true
  kitty_graphics: true
  sixel: false
  iterm2_images: false
  image_memory_limit_mib: 256
  image_transfer_limit_mib: 32
```

Map this shape to the **actual** structures in `src/config.rs` (the YAML is a proposed schema, not an existing one). Require separate per-transfer, per-image, and total-budget limits. Document `TERM` and graphics capability behavior rather than silently claiming `xterm-256color` implies image support.

---

# M1 — Parse OSC 133 inside Alacritty

## Step 4. Add a semantic operation to the vendored parser/terminal

Search the vendored code, then inspect the actual ANSI/VTE handler implementation:

```bash
rg 'osc_dispatch|Event::|impl.*Handler|Term<' vendor/alacritty_terminal
```

The desired control sequences are:

| Wire sequence (ST may be `ESC \\`) | Meaning |
|---|---|
| `ESC ] 133 ; A BEL` | prompt start |
| `ESC ] 133 ; B BEL` | prompt end / command input begins |
| `ESC ] 133 ; C BEL` | command execution begins |
| `ESC ] 133 ; D ; 0 BEL` | command finished with status 0 |
| `ESC ] 133 ; D ; 1 BEL` | command finished with status 1 |

Both `BEL` and `ST` terminate OSC; the **terminating BEL is not a visual-bell event**. Do not confuse it with a stand-alone BEL command.

Conceptual data type (place in an app-facing `src/terminal_semantics.rs` or vendored terminal module as appropriate):

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellMark {
    PromptStart,
    PromptEnd,
    CommandStart,
    CommandEnd { exit_code: Option<i32> },
}
```

At the OSC handler, check `params[0] == b"133"`; parse `A`, `B`, `C`, `D` and the optional exit code, and **record the marker at the current logical cursor position in the terminal state machine**. Unknown fields/actions should be ignored safely rather than panicking. Treat additional OSC 133 fields as optional extensions, not malformed input.

**Critical:** A listener event carrying only `ShellMark` is insufficient if the UI subsequently queries the live cursor; terminal output may have advanced. Capture the position *when the handler dispatches the mark*. For complex scrollback, associate marks with stable logical-line IDs or migrate with row movement. Avoid relying on an absolute visual row.

## Step 5. Store semantic state with tab lifetime

Suggested structure:

```rust
pub struct CommandBlock {
    pub id: u64,
    pub prompt: TerminalAnchor,
    pub input_start: Option<TerminalAnchor>,
    pub output_start: Option<TerminalAnchor>,
    pub output_end: Option<TerminalAnchor>,
    pub exit_code: Option<i32>,
}

pub struct SemanticState {
    pub blocks: std::collections::VecDeque<CommandBlock>,
    pub next_id: u64,
}
```

`TerminalAnchor` is an abstraction to implement, **not** an existing `alacritty_terminal` type. It must survive line wrapping, scrolling, scrollback eviction, resize/reflow, and alternate-screen switches. The initial version can use a grid position plus scroll generation to prove end-to-end parsing, but do not ship command navigation until anchors remain correct on reflow.

Attach the state to `Tab` or to the patched terminal state. If dispatching via `SpikeListener`, use a per-tab channel/queue, not one global map indexed by active tab; events arrive from independent PTY threads.

**Tests:** OSC A/B/C/D; missing status; negative/non-numeric status; split OSC across `advance()` buffers; multiple marks in one read; BEL versus ST; markers during alternate screen; malformed lengths.

---

# M2 — Emit OSC 133 from the shell

## Step 6. Extend the three existing shell scripts

Do **not** replace the scripts. They currently ring BEL on command failures and drive the window/tab attention pulse.

### Bash

Capture `$?` at the **start** of the `PROMPT_COMMAND` hook, before any other command overwrites it. Emit `D` with the exit code for the previous command and `A` for the next prompt. Emit `B` at the end of prompt rendering (usually via a nonprinting `PS1` suffix); emit `C` immediately before command execution using `DEBUG` trap **only with careful chaining and recursion guards**. Naive `trap DEBUG` implementations fire for every simple command and can corrupt prompt behavior.

An incremental Bash release can support A/B/D first; only enable C once you have a tested `DEBUG` strategy. Preserve an existing `PROMPT_COMMAND` array/string and any existing `DEBUG` trap. Test with Bash 5.x, multiline input, subshells, and ssh.

### Zsh

Use `add-zsh-hook precmd` to emit `D` and `A`, `preexec` to emit `C`, and `precmd`/prompt expansion to make `B` occur after the prompt has been drawn. Use `%{...%}` wrapping for nonprinting control sequences embedded in a Zsh prompt. Capture `$?` before invoking any extra command. Keep the existing failed-command BEL.

### Fish

Use `fish_preexec`/`fish_postexec` and an appropriate prompt function wrapper or event hook; maintain correct output order relative to the prompt. Keep existing `fish_postexec` BEL logic and avoid installing duplicate hooks when sourced twice.

**Do not emit untrusted command text** inside OSC 133. Marker metadata does not need to echo shell command content.

## Step 7. Build minimal command UX

Start with command blocks as an internal diagnostic overlay/log, then add "previous command" and "next command" actions to the existing `App` action/shortcut system in `src/main.rs`. Only add visual prompt or exit markers after verifying scrollback anchoring. Continue to use the existing BEL animation for failures; OSC 133 exit status can enrich the UI without duplicating alerts.

**Acceptance gate:** in each supported shell, commands are segmented accurately through success, failure, multiline editing, tab switching, and scrollback.

---

# M3 — Add Kitty Graphics Protocol parsing

## Step 8. Patch APC handling in the vendored Alacritty/VTE path

Kitty graphics uses Application Program Command (APC):

```text
ESC _ G <comma-separated key=value controls> ; <base64 payload> ESC \
```

Search for APC handling in the vendored VTE crate/Alacritty event loop. **Check whether `alacritty_terminal` re-exports `vte` or uses a transitive `vte` crate**. You may need to vendor/patch `vte` as well if its public handler silently discards APC without forwarding payload bytes. Do not assume an OSC hook will parse APC.

Recommended design: add a protocol operation invoked synchronously in the terminal engine, with the current cursor/grid state available. Maintain a persistent APC state machine across PTY read boundaries.

Minimum control handling:

- `a=t` transmit, `a=T` transmit + display, `a=p` place, `a=d` delete, `a=q` query (the exact wire semantics and defaults must follow the official spec).
- `f=100` PNG, `f=24` RGB and `f=32` RGBA (implement PNG first).
- `i` image ID, `p` placement ID, `c` columns, `r` rows, `z` z-index, `m` more-chunks flag, `q` quiet/response control.
- Support multiple chunks for one image, including base64-encoded payload split across reads; validate the last-chunk rules.
- Support APC terminator `ESC \\` and recovery from malformed/aborted sequences.
- Do not decode, allocate GPU textures, or block on filesystem reads from inside the parser lock.

Official normative reference: https://sw.kovidgoyal.net/kitty/graphics-protocol/

## Step 9. Make the parser output bounded operations

Use a bounded transfer assembler. After complete validation, send an owned operation into a per-tab queue for an image worker or renderer coordinator:

```rust
pub enum GraphicsOp {
    UploadPng { image_id: u32, bytes: Vec<u8> },
    Place { image_id: u32, placement: PlacementSpec },
    Delete { image_id: Option<u32>, placement_id: Option<u32> },
    // later: Query, UploadRgb, UploadRgba, Sixel, Iterm2
}
```

This is a **project-defined interface**. The real operations must retain enough Kitty metadata to honor offsets, relative/absolute positioning, background/text z-order, display options, and responses to the application.

For data safety: cap sequence size, chunk count, combined decoded bytes, decoded pixel count, integer conversions and image dimensions. Streaming base64 decoding can avoid excessive copies. Require a bounded queue; do not let a fast PTY producer allocate without backpressure. If responses are needed, route them back into the existing PTY writer (`Tab::write` / event-loop `Msg::Input`) without deadlock.

**Acceptance gate:** parser/unit tests reconstruct a PNG from artificially split reads and reject malformed/oversized APC sequences without crashing.

---

# M4 — Image model and terminal lifecycle

## Step 10. Build the CPU-side image store

Create:

```text
src/graphics/
    mod.rs
    kitty.rs
    store.rs
    placement.rs
    decode.rs
    tests.rs
```

A useful internal design:

```rust
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct ImagePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub anchor: TerminalAnchor,
    pub cols: u32,
    pub rows: u32,
    pub z_index: i32,
    pub source_rect: Option<PixelRect>,
}
```

`TerminalAnchor`, `PixelRect`, ID associations, and image cache eviction semantics are to be implemented. Separate **image data** from **placements**: the same decoded image may be displayed multiple times.

Store per-tab images and per-screen placements (primary vs alternate), not on the active window globally. When a `Tab` moves to another window, its store must move with it. When a tab dies, release its placements, pending transfers and GPU texture references.

## Step 11. Integrate with grid mutations

This is the core correctness work. In the patched terminal engine, identify where it:

- scrolls full/partial regions and changes scrollback offset;
- inserts/deletes lines;
- inserts/deletes/erases characters and clears regions;
- resizes/reflows lines and changes cell dimensions;
- switches between primary/alternate screen;
- resets terminal state and evicts scrollback.

Decide and test how anchored placements follow each operation. An image must not float at the old viewport coordinate after a scroll; a deleted placement must not reappear from GPU cache. Kitty's rules for image placement, removal and z-index override any convenient approximation.

**Initial scoping option:** implement visible-screen-only placements (and document that images do not survive scrolling into history) before supporting scrollback retention. This can be an internal milestone, not a claim of full Kitty compatibility.

## Step 12. Produce a render snapshot alongside `FrameData`

`App::build_frame()` in `src/main.rs` already locks the terminal, visits `term.renderable_content().display_iter`, and constructs owned `Run` records. Extend its `FrameData` to contain lightweight *visible image draws*, e.g.:

```rust
pub struct ImageDraw {
    pub image_id: u32,
    pub placement_id: u32,
    pub rect_px: [f32; 4],
    pub clip_px: [f32; 4],
    pub z_index: i32,
}
```

Compute coordinates in the terminal *content* area, using existing `self.cell_w`, `self.cell_h`, `strip_h`, and `display_offset`. Snapshot and release the terminal lock **before** image decoding and GPU work. Do not reference live `Term` cells during the wgpu render pass.

---

# M5 — Draw images using the existing wgpu 30 renderer

## Step 13. Introduce a GPU image atlas/cache

Create `src/graphics_gpu.rs` (or a `gpu/images.rs` module if you first refactor `gpu.rs`). Add to `Gpu` a texture cache keyed by `(tab_id, image_id, generation)` and an explicit maximum GPU memory budget. Avoid keying only by image ID: IDs may be reused in different tabs and after replacement.

For decoded RGBA8 image bytes, create a `wgpu::Texture` with `TEXTURE_BINDING | COPY_DST`, and upload it via `queue.write_texture` (or a staging buffer for large allocations). Use `Rgba8UnormSrgb` or `Rgba8Unorm` consistently with your shader's color handling; account for premultiplied alpha, and avoid double color conversion.

At minimum the pipeline needs:

- A bind group layout containing sampled 2D texture + sampler.
- A vertex buffer of textured quads (pixel or clip-space position + UV).
- A transparent-blending pipeline matching the existing surface format.
- A scissor rectangle matching terminal bounds, excluding the tab strip and overlays.
- Geometry batching to avoid one draw call per tiny image section.

Treat texture upload and rendering as separate stages so redraws do **not** re-upload unchanged images.

## Step 14. Insert image layers into `Gpu::render_frame()`

Current code in [`src/gpu.rs`](https://github.com/volca02/mypixels/blob/rust_rewrite/src/gpu.rs) has the order:

```text
1. draw_rects()
2. glyphon TextRenderer::render()
3. egui renderer
```

Target minimal order:

```text
1. terminal/background rects
2. image quads behind text
3. terminal cell backgrounds that must obscure images, where required
4. glyphon text
5. image quads above text (Kitty z-index)
6. cursor/selection/notifications as appropriate
7. egui last
```

**Caution:** `gpu.rects` currently mixes the window background, tab strip, cell backgrounds, URL underline, selection and cursor-related rectangles. Do not simply place *all* images after `draw_rects()` and declare z-order finished: it changes background, selection, cursor and z-index semantics. Split the draw list into explicit layers or first ship a documented behind-text subset.

Extend `render_frame()` to accept `&[ImageDraw]` alongside text areas (inspect its real signature/call sites before changing). Keep the egui overlay last. Ensure the preview/screenshot rendering path (`Gpu::ensure_previews`, offscreen jobs, screenshot readback) includes image draws if a screenshot or tab overview should show images; this is a **separate** code path and will not inherit screen images automatically.

**Acceptance gate:** one static PNG appears at the requested terminal cell, survives redraws without reupload, clips to terminal region, scrolls with its anchor, and does not cover the tab strip or egui settings panel.

---

# M6 — Complete the Kitty behavior

## Step 15. Implement lifecycle, responses, and negotiation

Implement and test the operations promised by the spec, not just image display:

- Image ID and placement ID allocation/reuse.
- Transmit-only versus transmit-and-place.
- Quiet vs normal response behavior and error responses, written back to the PTY.
- Deletion by ID, placement, position and other documented filters.
- PNG and raw RGB/RGBA, with dimensions validated before allocation.
- Chunked transfer and optional zlib compression.
- Crop/source rectangle, sizing in columns/rows/pixels, z-index.
- Query capability detection and accurate responses.
- Buffer reset/alternate-screen transitions and graceful resource eviction.

Read the official Kitty spec before assigning a meaning to any parameter; this checklist is not a replacement for it. For file/path/shared-memory transfer modes, decide explicitly whether to support them. If unsupported, return a protocol-appropriate error and avoid reading arbitrary paths from a terminal application's output.

**Important:** A graphics-aware application may probe support and refuse to send images if the terminal doesn't respond correctly. A renderer that understands uploads but never answers queries will seem broken to those applications.

---

# M7 — Sixel and iTerm2 adapters

## Step 16. Reuse the common store/renderer

Only after Kitty is reasonably stable:

- **Sixel:** parse DCS `q` syntax, color registers/palette, raster attributes, scrolling behavior, and image size limits. Feed `DecodedImage` + `ImagePlacement` to the shared store. Beware DCS parser state across PTY chunks.
- **iTerm2:** parse OSC 1337 `File=...` metadata, validate base64 payload and allowed inline image types, and translate dimensions/positioning into the same internal model. Implement inline display only before adding optional file transfer capabilities.

Useful references (port ideas rather than blindly merge):

- [ayosec/alacritty](https://github.com/ayosec/alacritty) — graphics-related Alacritty modifications; verify the branch and license before reuse.
- [Zed Alacritty OSC 133 branch](https://github.com/zed-industries/alacritty/tree/osc-133) — semantic shell integration ideas; do not assume it applies to your locked crate version.
- [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
- [FinalTerm / OSC 133 specification context](https://gitlab.freedesktop.org/terminal-wg/specifications).

Do not enable Sixel or iTerm2 capability advertising before the corresponding protocol works.

---

# M8 — Tests, limits and release gates

## Step 17. Add fixture-oriented parser tests

Create Rust test fixtures that advance the parser through:

1. Every possible byte split of a short OSC 133 and Kitty sequence.
2. Two commands in a single PTY chunk, with OSC text and graphics interleaved.
3. BEL and ST terminators, malformed APC escapes and partial EOF.
4. Base64 invalid characters, oversized payloads and untrusted dimensions.
5. Image replacement, placement reuse, deletion, tab close and alt-screen.
6. Scrolling inside full/partial scroll regions, resize/reflow and scrollback eviction.
7. Capability probes/responses, including quiet mode.
8. OSC 133 anchors across wrapped lines and cleared history.

Do *not* require wgpu/window creation for protocol tests; keep pure parser/store tests headless.

## Step 18. Add actual PTY smoke tests

Extend the existing `tests/` suite. Use the real application with a PTY program printing escape fixtures, verifying no hang/crash and visual output via the existing screenshot/preview path. Keep golden screenshots and test both normal and alternate screen. Build separate fixtures for image deletion, transparency, clipping and window resizing.

Useful manual smoke commands:

```bash
# OSC 133: print marker boundaries and a normal line
printf '\033]133;A\007prompt> \033]133;B\007echo ok\n\033]133;C\007ok\n\033]133;D;0\007'

# For Kitty graphics, run a test fixture/program that sends a real PNG
# through ESC _ G ... ESC \\ (do not print a textual placeholder).
```

After implementation, check popular emitting applications (for example, Kitty-compatible image viewers) using correct capability negotiation. Never assert compatibility from a single handcrafted sequence.

## Step 19. Performance + safety budgets

Track:

- Max encoded bytes per escape and per in-progress chunked transfer.
- Max decoded bytes/pixels per image, max aggregate CPU/GPU image memory.
- Limits per tab and globally, including offscreen tabs.
- Parser CPU usage under malformed or repeated escape sequences.
- GPU uploads/frame, draw calls/frame, avoid blocking PTY parse thread.
- No deadlocks between `FairMutex<Term<...>>`, graphics state lock, PTY sender and winit redraw thread.
- LRU/epoch-based reclamation when images go out of scope or tabs close.
- Accurate removal when image IDs are recycled; include generation in GPU cache key.

Commands:

```bash
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
# also run the existing project smoke and performance scripts
```

---

# Suggested PR/commit breakdown

| PR | Files (minimum) | Reviewable outcome |
|---|---|---|
| 1 | `Cargo.toml`, vendored Alacritty/VTE, `src/config.rs` | Reproducible patch foundation; no behavioral change |
| 2 | vendored parser, `src/terminal_semantics.rs`, `src/tab.rs`, unit tests | OSC 133 parsed and recorded with stable event ordering |
| 3 | `scripts/shell_integration.*`, `src/main.rs` | A/B/C/D generated; command blocks inspected and navigable |
| 4 | vendored APC handling, `src/graphics/kitty.rs`, tests | Kitty bounded decoding and correct protocol responses |
| 5 | `src/graphics/store.rs`, `src/tab.rs`, terminal lifecycle patches | Correct placement across grid operations |
| 6 | `src/gpu.rs`, `src/main.rs`, optional `src/graphics_gpu.rs` | wgpu draws images with proper layering and clipping |
| 7 | `src/graphics/sixel.rs`, `src/graphics/iterm.rs` | Optional protocols feeding same rendering infrastructure |
| 8 | `tests/`, benchmark scripts, `README.md` | Regression/compatibility gates and documented limits |

# Explicit non-goals for the first working version

- No full Kitty animation composition until single-frame images and placement are correct.
- No arbitrary host file reads or shared-memory transfer support without a carefully designed security policy.
- No guarantees for image retention in scrollback before stable anchors are implemented.
- No automatic inference of image support from `TERM=xterm-256color`.
- No attempt to merge whole graphics and OSC 133 forks onto current Alacritty without targeted compatibility review.

# Definition of done

The feature is complete when **OSC 133 A/B/C/D markers map to accurate per-tab command blocks**, Kitty clients can **negotiate support, upload/place/delete images**, and wgpu correctly composes images with terminal cells while scrolling, resizing, switching alternate screens and moving tabs. Existing BEL attention behavior, text shaping, previews/screenshots, and PTY throughput continue to work. Sixel and iTerm2 are independently feature-gated and verified rather than merely advertised.
