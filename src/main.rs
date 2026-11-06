//! mypixels (Rust port): tabbed terminal.
//! alacritty_terminal (emulation + pty) rendered with glyphon (cosmic_text)
//! on wgpu in a winit window.
//!
//! Usage: mypixels-rs [command [args...]]   (defaults to $SHELL or bash)

mod clipboard;
mod config;
mod egui_ui;
mod ui_style;
mod tab_bar;
mod fonts;
mod gpu;
mod kitty;
mod kitty_kb;
mod mcp;
mod tab;
mod session;
mod theme;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::Msg;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;

use glyphon::{
    Attrs, Buffer, FontSystem, Metrics, Resolution, Shaping, TextArea, TextBounds,
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::wayland::WindowAttributesExtWayland as _;
use winit::window::{Window, WindowId};

use config::Config;
use gpu::Gpu;
use tab::{SemanticMark, SpikeListener, Tab};

const HISTORY: usize = 10_000;
const MAX_RUNS: usize = 4096;
const PALETTE: [[f32; 3]; 16] = [
    [0.0, 0.0, 0.0], [0.8, 0.2, 0.2], [0.2, 0.6, 0.2], [0.6, 0.6, 0.2],
    [0.2, 0.2, 0.8], [0.6, 0.2, 0.6], [0.2, 0.6, 0.6], [0.8, 0.8, 0.8],
    [0.4, 0.4, 0.4], [1.0, 0.4, 0.4], [0.4, 1.0, 0.4], [1.0, 1.0, 0.4],
    [0.4, 0.4, 1.0], [1.0, 0.4, 1.0], [0.4, 1.0, 1.0], [1.0, 1.0, 1.0],
];


fn cube_color(i: u8) -> [f32; 3] {
    let i = (i - 16) as usize;
    let (r, g, b) = (i / 36, (i / 6) % 6, i % 6);
    let c = |v: usize| if v == 0 { 0.0 } else { (55.0 + 40.0 * v as f32) / 255.0 };
    [c(r), c(g), c(b)]
}

#[derive(Clone, Copy)]
struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows + HISTORY
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone)]
struct Palette {
    colors: Box<[Option<[f32; 3]>]>,
    /// 16-color palette from the config; used when the terminal itself has
    /// not overridden a slot at runtime (OSC 4).
    cfg16: Option<[[f32; 3]; 16]>,
}

impl Palette {
    fn from_colors(
        colors: &alacritty_terminal::term::color::Colors,
        cfg16: &Option<[config::Rgb; 16]>,
    ) -> Self {
        let mut v = Vec::with_capacity(269);
        for i in 0..269 {
            v.push(match &colors[i] {
                Some(rgb) => Some([rgb.r as f32 / 255.0, rgb.g as f32 / 255.0, rgb.b as f32 / 255.0]),
                None => None,
            });
        }
        let cfg16 = cfg16.as_ref().map(|p| p.iter().map(|c| c.to_f32()).collect::<Vec<_>>().try_into().unwrap());
        Self { colors: v.into(), cfg16 }
    }

    // Resolve a cell color; fg/bg supplied from config for the default
    // foreground/background/cursor named colors.
    fn resolve_with(&self, c: &alacritty_terminal::vte::ansi::Color, fg: [f32; 3], bg: [f32; 3], cursor: [f32; 3]) -> [f32; 3] {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        match c {
            Color::Spec(rgb) => [rgb.r as f32 / 255.0, rgb.g as f32 / 255.0, rgb.b as f32 / 255.0],
            Color::Named(n) => {
                let i = *n as usize;
                if i < 269 {
                    if let Some(rgb) = &self.colors[i] {
                        return *rgb;
                    }
                }
                if i < 16 {
                    match &self.cfg16 {
                        Some(p) => p[i],
                        None => PALETTE[i],
                    }
                } else if i == NamedColor::Foreground as usize {
                    fg
                } else if i == NamedColor::Background as usize {
                    bg
                } else if i == NamedColor::Cursor as usize {
                    cursor
                } else if *n == NamedColor::BrightForeground {
                    fg
                } else if *n == NamedColor::DimForeground {
                    fg.map(|v| v * 0.66)
                } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) {
                    let slot = i - NamedColor::DimBlack as usize;
                    self.cfg16.as_ref().unwrap_or(&PALETTE)[slot].map(|v| v * 0.66)
                } else {
                    fg
                }
            },
            Color::Indexed(i) => {
                let j = *i as usize;
                if j < 269 {
                    if let Some(rgb) = &self.colors[j] {
                        return *rgb;
                    }
                }
                if *i < 16 {
                    match &self.cfg16 {
                        Some(p) => p[*i as usize],
                        None => PALETTE[*i as usize],
                    }
                } else if (16..232).contains(i) {
                    cube_color(*i)
                } else {
                    let v = (8.0 + 10.0 * (*i as u32 - 232) as f32) / 255.0;
                    [v, v, v]
                }
            },
        }
    }
}


// ---------------------------------------------------------------------------
// Draw list (owned data copied out of the terminal under the lock)
// ---------------------------------------------------------------------------

/// What a run's shaped form depends on: the text, both colours, the font it is
/// drawn with, and the width its buffer was sized to. Equal signatures mean the
/// pooled glyph buffer is still exactly right, so shaping can be skipped -- and
/// shaping is by far the most expensive part of a frame here.
fn run_sig(run: &Run, cell_w: f32) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x100000001b3);
    };
    for b in run.text.as_bytes() {
        mix(*b as u64);
    }
    for &(byte, column) in &run.cells {
        mix(byte as u64);
        mix(column as u64);
    }
    // Colour is part of the identity: a TUI repaints the same text in different
    // colours, and a stale colour is a visible bug.
    for c in run.fg.iter().chain(run.bg.iter()) {
        mix((c.clamp(0.0, 1.0) * 255.0).round() as u64);
    }
    mix((cell_w * 1000.0).round() as u64);
    mix(run.width as u64);
    mix(run.attrs as *const Attrs<'static> as u64);
    h
}

/// Escape hatch to A/B the shaping cache against re-shaping everything
/// (`MYPIXELS_NOCACHE=1`).
fn shaping_cache_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MYPIXELS_NOCACHE").is_none())
}

/// Append a run, dropping the ones that could never draw anything: padding in
/// the default colours. A mostly-empty 4K grid is 80k cells, and this is the
/// difference between thousands of dead runs and the handful that matter. Runs
/// carrying a background colour are kept -- that is how a TUI's highlighted row
/// still shows.
fn push_run(runs: &mut Vec<Run>, mut r: Run, _def_fg: [f32; 3], def_bg: [f32; 3]) {
    // Keep the full width for background painting, but never shape invisible
    // end-of-line padding. A 4K find listing otherwise shapes ~75k glyphs per
    // refresh, most of them spaces after the path.
    r.text.truncate(r.text.trim_end_matches(' ').len());
    r.cells.truncate(r.cells.partition_point(|(byte, _)| *byte < r.text.len()));
    if r.bg == def_bg && r.text.is_empty() {
        return;
    }
    runs.push(r);
}

struct Run {
    row: u32,
    col: u32,
    width: u32,
    fg: [f32; 3],
    bg: [f32; 3],
    text: String,
    /// UTF-8 cluster origins mapped to terminal columns within this run.
    cells: Vec<(usize, u32)>,
    /// Font for this run. Runs split where the required face changes, so a
    /// starship prompt becomes "text | icon | text" instead of "text | tofu".
    attrs: &'static Attrs<'static>,
}

#[derive(Default)]
struct FrameData {
    fg: [f32; 3],
    bg: [f32; 3],
    cursor_color: [f32; 3],
    runs: Vec<Run>,
    cursor: Option<(u32, u32)>,
}

/// The Alt-held visual tab switcher. Standalone overview is intentionally absent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OverviewMode {
    /// Alt-held popup: follows the active tab, dismissed on Alt release.
    Switcher,
}

/// The highlighted tile mirrors the active tab; releasing Alt commits it.
#[derive(Clone, Copy)]
struct OverviewState {
    mode: OverviewMode,
    selected: usize,
    /// Escape restores the tab that was active before this gesture.
    original: usize,
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// One read-back, uploaded switcher thumbnail: the egui texture, the pixel
/// size it was read at, the preview content signature it matches, and when it
/// was last read (for the throttle).
#[derive(Clone)]
struct SwThumb {
    handle: egui::TextureHandle,
    size: (u32, u32),
    pixels: (u32, u32),
    sig: u64,
    last: std::time::Instant,
}

/// Everything that lives "in a window": the surface and its GPU, this
/// window's egui state, the tab strip as this window displays it, and the
/// per-window input/selection/switcher state. Windows in one process share
/// the config, the fonts, the clipboard and the session bus -- and moving a
/// tab between windows is a Vec::remove plus a Vec::push, which is exactly
/// why the windows live in one process at all: a terminal is its pty, and a
/// pty cannot cross a process boundary.
struct Win {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    /// egui side: config panel, switcher overlay, rename box -- one context
    /// per window on purpose: shared egui contexts leak hover state between
    /// windows, and two independent contexts cost only a font atlas.
    egui: Option<egui_ui::Egui>,
    tabs: Vec<Tab>,
    active: usize,
    dims: Dims,
    /// This window's grid. The App-level rows/cols are the template a new
    /// window is sized from; these are what is actually on screen.
    cols: usize,
    rows: usize,
    tab_hits: Vec<(f32, f32)>,
    add_hit: (f32, f32),
    tab_close_hits: Vec<egui::Rect>,
    tab_close_press: Option<usize>,
    pointer: (f32, f32),
    mouse_button: Option<MouseButton>,
    mouse_last: Option<(u16, u16)>,
    drag: Option<(Point, Side)>,
    click: Option<(u8, u32, (f32, f32), std::time::Instant)>,
    wheel_pixels: f32,
    hover_tab: usize,
    hover_add: bool,
    tablist_hidden: bool,
    /// The egui rename box is open: keys belong to it, not the terminal.
    renaming: bool,
    /// Alt-held visual switcher (only ever the focused window's; the Alt
    /// key itself is a global modifier).
    overview: Option<OverviewState>,
    sw_thumbs: Vec<Option<SwThumb>>,
    thumb_rr: usize,
    /// last BEL seen anywhere (visual bell; see bell_flash)
    autoscroll_next: Option<Instant>,
    /// One-shot: the next frame renders the active tab through the preview
    /// path and copies the result to the clipboard as a PNG.
    shot: bool,
    tab_drag: Option<usize>,
    /// The tab drag crossed into another cell, so the press has become a
    /// reorder and the release must not also count as a click.
    tab_drag_moved: bool,
    /// egui events synthesized from FIFO key specs, queued for the next
    /// RawInput.
    fifo_egui_events: Vec<egui::Event>,
    /// Per-window error/status bubble (see show_error).
    notice: Option<(String, Instant)>,
    /// Winit's focus report. The switcher and the attention request consult
    /// it, so a background window's blink never steals a foreground gesture.
    /// Tab cell ringing a red pulse after a BEL arrived unseen (drain_bell_pings).
    bell_pulse: Option<(usize, Instant)>,
    focused: bool,
}

struct App {
    config: Config,
    /// The windows of this session, in creation order. `cur` is the one the
    /// event being handled belongs to (set by the router at the top of
    /// `window_event`); "current window" is what every helper below means.
    wins: Vec<Win>,
    cur: usize,
    next_id: usize,
    cell_w: f32,
    cell_h: f32,
    tab_char_w: f32,
    tab_h: f32,
    tab_text_h: f32,
    /// Window scale factor, applied to every metric above. The whole render
    /// pipeline is in *physical* pixels (surface size, pointer and wheel
    /// events all are), so point sizes are converted to pixels with this
    /// factor: on a 2x display a 10pt font rasterises at double the physical
    /// size and a cell is double, while the grid and hit-testing work in the
    /// same units as before. `App::new` computes the metrics at 1.0 because
    /// no window exists yet; `resumed` applies the real factor before the
    /// window is created and `ScaleFactorChanged` keeps it current.
    scale: f32,
    /// Vertical metrics of the term/tablist fonts, read from the fonts
    /// themselves -- line height differs a lot per family (Cascadia 9pt lands on
    /// 15px, Terminus 12 on 17px) and one guessed factor fit neither.
    term_metrics: fonts::LineMetrics,
    tab_metrics: fonts::LineMetrics,
    /// New-window grid template (and the very first window's size): App keeps
    /// a cols/rows pair even though every window owns its real grid.
    cols: usize,
    rows: usize,
    clipboard: Option<clipboard::Clipboard>,
    theme_watcher: Option<theme::Watcher>,
    fg: [f32; 3],
    bg: [f32; 3],
    exited: Arc<Mutex<Vec<usize>>>,
    /// Every live window, for the wake closures that outlive any single one
    /// (pty listeners, clock, clipboard, theme watcher): a redraw request
    /// goes to all of them; drawing the wrong one is cheap and correct.
    window_slot: Arc<Mutex<Vec<Arc<Window>>>>,
    font_system: FontSystem,
    mono_attrs: Attrs<'static>,
    tab_attrs: Attrs<'static>,
    tab_face_key: String,
    /// Per-codepoint font routing, rebuilt whenever the terminal font changes.
    coverage: RefCell<Option<fonts::Coverage>>,
    /// Runtime terminal font state. `term_pt` is the live size (enlarge/
    /// shrink font mutates it); a config reload resets it to the config value.
    /// The family here is the RESOLVED one: a config spec of "Foo Bold 12"
    /// stores family "Foo" plus its style, so rebuilds do not re-parse.
    term_family: &'static str,
    term_weight: u16,
    term_slant: fonts::DbStyle,
    term_pt: f32,
    /// The size the terminal font was loaded at: what "Reset zoom" returns
    /// to, immune to whatever the config's string happens to say after a
    /// zoom-and-save cycle this session.
    orig_term_pt: f32,
    /// Alt-held tab switcher timing, shared with a background clock thread.
    /// `switcher_alt_at` is when Alt went down (None when released); the clock
    /// opens the switcher after the configured delay (`tab_overview_delay_ms`)
    /// and, while it's open, nudges a redraw on a fixed cadence so every
    /// thumbnail stays live. `switcher_show_req` is the clock's "show now"
    /// signal; `switcher_open` mirrors the open state for the clock.
    switcher_alt_at: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    switcher_show_req: Arc<AtomicBool>,
    switcher_open: Arc<AtomicBool>,
    /// Cursor-blink phase (toggled by the clock thread); render draws the
    /// cursor only when this is set.
    cursor_blink: Arc<AtomicBool>,
    /// Last terminal output time (shared with the pty listeners); the clock
    /// thread reads it to keep the cursor solid right after activity.
    last_activity: Arc<std::sync::Mutex<std::time::Instant>>,
    /// last BEL seen anywhere (visual bell; see bell_flash)
    bell: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// every BEL as (tab_id, time), drained into attention + pulses
    bell_pings: Arc<std::sync::Mutex<Vec<(usize, std::time::Instant)>>>,
    /// OSC 133 marks as dispatched, drained into per-tab command blocks
    sems_ring: Arc<std::sync::Mutex<Vec<SemanticMark>>>,
    /// Opens an order stamp per command (see `CommandBlock::seq`).
    sem_seq: u64,
    /// Complete APC strings awaiting graphics interpretation.
    apcs_ring: Arc<std::sync::Mutex<Vec<(usize, Vec<u8>, usize, usize)>>>,
    pty_writes_ring: Arc<std::sync::Mutex<Vec<(usize, String)>>>,
    resets_ring: Arc<std::sync::Mutex<Vec<usize>>>,
    cell_sizes_ring: Arc<std::sync::Mutex<Vec<usize>>>,
    /// MYPIXELS_APC_DUMP=<file>: append every graphics frame verbatim for
    /// offline replay (tests a real sender's exact bytes against the
    /// parser). Debug hook, off by default.
    apc_dump: Option<std::path::PathBuf>,
    /// Per-tab Kitty graphics state, keyed by stable tab id; created on
    /// first use, removed when the tab dies.
    kitty: std::collections::HashMap<usize, kitty::TabKitty>,
    /// Per-tab command blocks, oldest first. Owned by the app, keyed by
    /// the stable tab id, so a tab that moves between windows keeps its
    /// history and a tab that dies simply leaves no trace.
    sem_blocks: std::collections::HashMap<usize, std::collections::VecDeque<CommandBlock>>,
    /// Daemon: tab/rise requests forwarded by other mypixels processes.
    session_rx: std::sync::mpsc::Receiver<session::Request>,
    mcp_rx: std::sync::mpsc::Receiver<mcp::Query>,
    /// The egui window picker's answer, applied next frame by the drain
    /// (which has the event loop; the draw path does not). usize::MAX =
    /// detach into a fresh window.
    /// One small expanding ring at the cursor's cell, re-armed whenever the
    /// cursor actually moves cells. That is the whole of it: a 180 ms pulse
    /// so the eye can keep the pointer's agent when it slides across the
    /// grid (mouse-reporting apps, search jumps, wide terminals).
    ripple: Option<Ripple>,
    /// Closed tabs, most recent last: (window title at death, working
    /// directory at death). Ctrl+Shift+z respawns the newest from here, in
    /// the window that asks. Bounded - a reopen history is a memory of the
    /// last few minutes, not a session manager; that is a different,
    /// harder promise, and half-measures should not pretend at it.
    closed_ring: std::collections::VecDeque<(String, Option<std::path::PathBuf>)>,
    /// URLs on the row under the pointer (only that row can underline or
    /// be clicked); reparsed only when the row's content hash moves.
    url_spans: Vec<UrlSpan>,
    /// (row, content hash, spans) of the last row scanned, for the reuse
    /// fast path in `scan_row_cached`.
    url_cache: Option<(usize, u64, Vec<UrlSpan>)>,
    /// Index into url_spans under the pointer, for the underline.
    url_hover: Option<usize>,
    /// Where the cursor block was painted last frame, physical px, with its
    /// resolved colour. Key events need it without going through the frame
    /// builder: the ripple rings AT the cursor when a modifier is pressed.
    cursor_pt: Option<([f32; 2], [f32; 3])>,
    pending_move: Option<usize>,
    /// A context-menu choice, run by the drain (which has the event loop).
    pending_action: Option<String>,
    /// Keeps the session-bus thread -- and with it the bus name -- alive for
    /// as long as the app runs.
    _session: Option<session::Server>,
    modifiers: ModifiersState,
    program: String,
    args: Vec<String>,
    /// Test hook: receiver of key specs fed from the $MYPIXELS_KEYS FIFO by a
    /// reader thread (about_to_wait is rarely called on Wayland).
    key_rx: Option<std::sync::mpsc::Receiver<String>>,
}

impl App {
    fn new() -> Self {
        let config = Config::load();
        // Pango sizes are points; cosmic-text wants pixels (@96 dpi). The
        // parsed family may still carry style words ("Foo Bold 12");
        // resolve_font() splits those against the real database below.
        let (term_spec, term_pt) = config::parse_font(&config.term_font);
        let (tab_spec, tab_pt) = config::parse_font(&config.tablist_font);
        // Fallback chain must know the primary families up front, so powerline /
        // Nerd Font glyphs in prompts resolve to icons instead of tofu.
        let (mut font_system, fallback_chain) =
            fonts::build_font_system(&[term_spec.as_str(), tab_spec.as_str()]);
        log::info!("font fallback chain: {:?}", fallback_chain);
        let font_px = term_pt * 96.0 / 72.0;
        let tab_font_px = tab_pt * 96.0 / 72.0;
        // Font size comes from the Buffer Metrics (see Gpu::init), not Attrs.
        // mono_attrs/tab_attrs are 'static, so the family names must be too.
        let term = fonts::resolve_font(font_system.db(), &term_spec);
        let tab = fonts::resolve_font(font_system.db(), &tab_spec);
        let term_family: &'static str = Box::leak(term.family.into_boxed_str());
        let tab_family: &'static str = Box::leak(tab.family.into_boxed_str());
        let tab_face_key = egui_ui::preview_family_name(tab_family, tab.weight, tab.slant);
        let (term_weight, term_slant) = (term.weight, term.slant);
        let (mono_attrs, tab_attrs) = {
            let db = font_system.db();
            (
                fonts::pin_face(
                    db,
                    Attrs::new(),
                    term_family,
                    term_weight,
                    term_slant,
                ),
                fonts::pin_face(
                    db,
                    Attrs::new(),
                    tab_family,
                    tab.weight,
                    tab.slant,
                ),
            )
        };

        // Line height comes from the font itself, not a guessed factor.
        let term_metrics = fonts::line_metrics(&mut font_system, term_family, term_weight, term_slant)
            .unwrap_or(fonts::LineMetrics::DEFAULT);
        let tab_metrics = fonts::line_metrics(&mut font_system, tab_family, tab.weight, tab.slant)
            .unwrap_or(fonts::LineMetrics::DEFAULT);
        let term_lh = term_metrics.height_at(font_px);
        let tab_lh = tab_metrics.height_at(tab_font_px);

        let mut measure = Buffer::new(&mut font_system, Metrics::new(font_px, term_lh));
        measure.set_size(Some(800.0), None);
        measure.set_text("MMMM", &mono_attrs, Shaping::Basic, None);
        measure.shape_until_scroll(&mut font_system, false);
        let shaped = measure.lines[0].shape(&mut font_system, 8);
        let mut total = 0.0;
        for span in &shaped.spans {
            for word in &span.words {
                total += word.width(font_px);
            }
        }
        // Whole-pixel cell grid. A fractional advance puts every glyph on a
        // sub-pixel origin, so each stem is antialiased twice: text reads
        // heavier than the original, and solid shapes (powerline separators,
        // block glyphs) pick up a fringe along their edges. VTE snaps cells to
        // integers; matching that is also what keeps columns within a pixel or
        // two of the reference at the same point size.
        let cell_w = (total / 4.0).round().max(1.0);
        let cell_h = term_lh.round().max(1.0);

        let mut tmeasure =
            Buffer::new(&mut font_system, Metrics::new(tab_font_px, tab_lh));
        tmeasure.set_size(Some(800.0), None);
        tmeasure.set_text("MMMM", &tab_attrs, Shaping::Basic, None);
        tmeasure.shape_until_scroll(&mut font_system, false);
        let tshaped = tmeasure.lines[0].shape(&mut font_system, 8);
        let mut ttotal = 0.0;
        for span in &tshaped.spans {
            for word in &span.words {
                ttotal += word.width(tab_font_px);
            }
        }
        let tab_char_w = (ttotal / 4.0).max(1.0);
        let tab_h = ui_style::tab_height(tab_lh, 1.0);

        log::info!(
            "font: term {term_family} {term_pt}pt, tablist {tab_family} {tab_pt}pt; cell = {cell_w:.2} x {cell_h:.2}, tab char = {tab_char_w:.2}, strip = {tab_h:.1}px"
        );

        let fg = config.foreground_color.to_f32();
        let bg = crate::config::terminal_bg(&config).to_f32();

        let (program, args) = term_command();

        let window_slot = Arc::new(Mutex::new(Vec::new()));
        let key_rx = spawn_key_reader(Arc::clone(&window_slot));
        // Daemon mode: claim this display's session name unless asked for a
        // private instance. Another `mypixels` run will find us here and open
        // its tab in *this* window instead of growing another frame; its
        // request arrives on this channel (see the RedrawRequested drain).
        let standalone = std::env::args()
            .skip(1)
            .take_while(|a| a.starts_with('-'))
            .any(|a| a == "--standalone" || a == "-s");
        let (session_tx, session_rx) = std::sync::mpsc::channel();
        let (mcp_tx, mcp_rx) = std::sync::mpsc::channel();
        let _session = if standalone {
            None
        } else {
            session::bus_name()
                .and_then(|name| {
                    let slot = Arc::clone(&window_slot);
                    session::start(&name, session_tx, move || {
                        if let Ok(slot) = slot.lock() {
                            for w in slot.iter() {
                                w.request_redraw();
                            }
                        }
                    })
                })
        };
        // LLM observability: read-only MCP endpoint on a per-display unix
        // socket (windows, tabs, captured command output). Same
        // standalone carve-out as the session bus.
        let _mcp = if standalone {
            None
        } else {
            let slot = Arc::clone(&window_slot);
            mcp::start(mcp_tx, Arc::new(move || {
                if let Ok(slot) = slot.lock() {
                    for w in slot.iter() {
                        w.request_redraw();
                    }
                }
            }))
        };
        let shared = SpikeListener {
            window: Arc::clone(&window_slot),
            tab_id: 0,
            title: Arc::new(Mutex::new(String::new())),
            exited: Arc::new(Mutex::new(Vec::new())),
            last_activity: Arc::new(Mutex::new(std::time::Instant::now())),
            bell: Arc::new(Mutex::new(None)),
            bells: Arc::new(Mutex::new(Vec::new())),
            sems: Arc::new(Mutex::new(Vec::new())),
            apcs: Arc::new(Mutex::new(Vec::new())),
            pty_writes: Arc::new(Mutex::new(Vec::new())),
            resets: Arc::new(Mutex::new(Vec::new())),
            cell_sizes: Arc::new(Mutex::new(Vec::new())),
        };
        let bells = Arc::clone(&shared.bells);
        let sems = Arc::clone(&shared.sems);
        let apcs = Arc::clone(&shared.apcs);
        let pty_writes = Arc::clone(&shared.pty_writes);
        let resets = Arc::clone(&shared.resets);
        let cell_sizes = Arc::clone(&shared.cell_sizes);
        let exited = Arc::clone(&shared.exited);
        let last_activity = Arc::clone(&shared.last_activity);
        let bell = Arc::clone(&shared.bell);
        // Cursor blink phase, toggled by the clock thread (activity-aware).
        let cursor_blink = Arc::new(AtomicBool::new(true));

        // Alt-held switcher clock. Shared with the event-loop thread.
        let switcher_alt_at = Arc::new(Mutex::new(None));
        let switcher_show_req = Arc::new(AtomicBool::new(false));
        let switcher_open = Arc::new(AtomicBool::new(false));
        {
            let ws = Arc::clone(&window_slot);
            let alt_at = Arc::clone(&switcher_alt_at);
            let show_req = Arc::clone(&switcher_show_req);
            let open = Arc::clone(&switcher_open);
            let blink = Arc::clone(&cursor_blink);
            let last_act = Arc::clone(&last_activity);
            let delay_ms = config.tab_overview_delay_ms;
            let enabled = config.tab_overview_on_alt;
            let refresh_ms = 150u32; // thumbnail refresh cadence while open
            let blink_ms = 533u128; // cursor on/off cadence
            std::thread::spawn(move || {
                let mut last_refresh = std::time::Instant::now();
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    let now = std::time::Instant::now();
                    // Cursor blink: solid right after activity, then blinks in
                    // blink_ms on/off steps measured from last activity.
                    let idle = now.duration_since(*last_act.lock().unwrap());
                    let visible = if idle.as_millis() < blink_ms {
                        true
                    } else {
                        (idle.as_millis() / blink_ms) % 2 == 0
                    };
                    if blink.load(Ordering::SeqCst) != visible {
                        blink.store(visible, Ordering::SeqCst);
                        for w in ws.lock().unwrap().iter() {
                            w.request_redraw();
                        }
                    }
                    if !open.load(Ordering::SeqCst) {
                        // Open after the configured Alt hold delay.
                        if let Some(at) = *alt_at.lock().unwrap() {
                            if enabled && now.duration_since(at).as_millis() >= delay_ms as u128
                                && !show_req.swap(true, Ordering::SeqCst)
                            {
                                for w in ws.lock().unwrap().iter() { w.request_redraw(); }
                            }
                        }
                    } else if now.duration_since(last_refresh).as_millis() >= refresh_ms as u128 {
                        // Keep every preview live while the switcher is open.
                        last_refresh = now;
                        for w in ws.lock().unwrap().iter() {
                            w.request_redraw();
                        }
                    }
                }
            });
        }

        // Initial grid from the configured window size; the Resized handler
        // adjusts it once the surface exists.
        let cols = ((config.default_width as f32 / cell_w) as usize).clamp(20, 512);
        let rows = (((config.default_height as f32 - tab_h) / cell_h) as usize).clamp(10, 400);

        App {
            wins: Vec::new(),
            cur: 0,
            config,
            next_id: 0,
            cols,
            rows,
            cell_w,
            cell_h,
            tab_char_w,
            tab_h,
            tab_text_h: tab_lh,
            scale: 1.0,
            term_metrics,
            tab_metrics,
            clipboard: None,
            theme_watcher: None,
            fg,
            bg,
            exited,
            window_slot,
            font_system,
            mono_attrs,
            tab_attrs,
            tab_face_key,
            coverage: RefCell::new(None),
            term_family,
            term_weight,
            term_slant,
            term_pt,
            orig_term_pt: term_pt,
            switcher_alt_at,
            switcher_show_req,
            switcher_open,
            cursor_blink,
            last_activity,
            bell,
            bell_pings: bells,
            sems_ring: sems,
            sem_seq: 0,
            apcs_ring: apcs,
            pty_writes_ring: pty_writes,
            resets_ring: resets,
            cell_sizes_ring: cell_sizes,
            apc_dump: std::env::var("MYPIXELS_APC_DUMP").ok().map(std::path::PathBuf::from),
            kitty: Default::default(),
            sem_blocks: Default::default(),
            session_rx,
            mcp_rx,
            pending_move: None,
            pending_action: None,
            ripple: None,
            cursor_pt: None,
            closed_ring: std::collections::VecDeque::new(),
            url_spans: Vec::new(),
            url_cache: None,
            url_hover: None,
            _session,
            modifiers: ModifiersState::empty(),
            program,
            args,
            key_rx,
        }
    }

    fn init_gpu(&mut self, event_loop: &ActiveEventLoop, window: &Arc<Window>) {
        // Live sizes, not the config's: `term_pt` carries any zoom applied this
        // session, `self.scale` the display's factor, and cell_h the rounded
        // terminal line height, so the glyph buffers match the live grid.
        let term_px = self.term_pt * 96.0 / 72.0 * self.scale;
        let gpu = Gpu::init(
            event_loop,
            window,
            &mut self.font_system,
            term_px,
            self.cell_h,
        );
        // The font popup previews from the app's own database, so the list
        // is exactly what the terminal can actually load.
        let font_choices = fonts::font_choices(self.font_system.db());
        self.wins[self.cur].egui = Some(egui_ui::Egui::init(
            event_loop,
            &gpu.device,
            &gpu.queue,
            gpu.config.format,
            self.scale,
            font_choices,
        ));
        self.wins[self.cur].gpu = Some(gpu);
    }

    fn add_tab(&mut self) {
        self.add_tab_with(None, None);
    }

    /// Open a tab, optionally overriding where it starts (`cwd`) and what it
    /// runs. The daemon's OpenTab uses both: a tab beside the shell that ran
    /// `mypixels`, in that shell's directory, doing what it asked.
    fn add_tab_with(
        &mut self,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<(String, Vec<String>)>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let window_size = WindowSize {
            num_lines: self.wins[self.cur].rows as u16,
            num_cols: self.wins[self.cur].cols as u16,
            cell_width: self.cell_w as u16,
            cell_height: self.cell_h as u16,
        };
        let (program, args) = match cmd {
            Some(c) => c,
            None => (self.program.clone(), self.args.clone()),
        };
        let tab = Tab::new(
            id,
            &self.wins[self.cur].dims,
            window_size,
            program,
            args,
            &self.listener(),
            self.config.scrollback_lines,
            self.config.kitty_compatible,
            cwd,
            // The shell learns WHICH window of the session it lives in, so a
            // `mypixels` typed in it joins that window, not window 1. 1-based
            // because 0 in the environment means "standalone, stay out".
            self.cur as u32 + 1,
        );
        self.wins[self.cur].tabs.push(tab);
        self.wins[self.cur].active = self.wins[self.cur].tabs.len() - 1;
    }

    /// Handle what other mypixels processes asked of this session.
    /// Remove a window from the session: the wake-slot list first (dead
    /// windows must not be woken, nor kept alive by it), then the window
    /// itself; the last one exiting ends the session.
    /// Drop a dead tab's textures from EVERY window's cache: a tab that was
    /// once previewed or shown in another window left copies there, and the
    /// key (a never-reused id) would never be overwritten.
    fn forget_gpu_images(&mut self, tab_id: usize) {
        for w in self.wins.iter_mut() {
            if let Some(g) = w.gpu.as_mut() {
                g.forget_images(tab_id as u32);
            }
        }
    }

    fn close_window(&mut self, i: usize, event_loop: &ActiveEventLoop) {
        // A window dying takes its tabs with it; remember them all, oldest
        // first, so Ctrl+Shift+z can walk them back one by one.
        let deaths: Vec<_> = self.wins[i].tabs.iter().map(tab_death).collect();
        let dying: Vec<usize> = self.wins[i].tabs.iter().map(|t| t.id).collect();
        self.kitty.retain(|id, _| !dying.contains(id));
        for id in dying {
            self.forget_gpu_images(id);
        }
        for d in deaths {
            self.remember_closed(d);
        }
        if let Some(w) = self.wins[i].window.as_ref() {
            let id = w.id();
            self.window_slot.lock().unwrap().retain(|w| w.id() != id);
        }
        self.wins.remove(i);
        self.cur = self.cur.min(self.wins.len().saturating_sub(1));
        if self.wins.is_empty() {
            event_loop.exit();
        }
    }

    /// Hand a tab from one window to another. The tab object itself moves:
    /// pty, child, terminal state, nothing respawned. The view follows the
    /// tab (the user should see where it went, and an unfocused target
    /// asks for attention), and a source window emptied of tabs closes.
    /// The moved shell's $MYPIXELS_WINDOW still names the OLD window --
    /// environment of a running process cannot change -- so `mypixels`
    /// typed there afterwards opens tabs in the window it left. Known wart.
    fn move_tab(
        &mut self,
        from: usize,
        idx: usize,
        to: usize,
        insert: usize,
        event_loop: &ActiveEventLoop,
    ) {
        if from == to || to >= self.wins.len() || idx >= self.wins[from].tabs.len() {
            return;
        }
        let insert = insert.min(self.wins[to].tabs.len());
        let flen = self.wins[from].tabs.len();
        let active = self.wins[from].active;
        let tab = self.wins[from].tabs.remove(idx);
        log::info!("moved tab {:?} to window {}", tab.display_title(), to + 1);
        self.wins[to].tabs.insert(insert, tab);
        let emptied = self.wins[from].tabs.is_empty();
        if !emptied {
            self.wins[from].active = move_active_follow(flen, active, idx).unwrap_or(0);
        }
        let mut cur = to;
        if emptied {
            if to > from {
                cur = to - 1;
            }
            self.close_window(from, event_loop);
        }
        self.cur = cur;
        self.wins[cur].active = insert;
        if let Some(w) = self.wins[cur].window.as_ref() {
            if !self.wins[cur].focused {
                w.request_user_attention(Some(winit::window::UserAttentionType::Critical));
            }
        }
        for w in &self.wins {
            if let Some(win) = &w.window {
                win.request_redraw();
            }
        }
    }

    /// Which of this session's windows contains a GLOBAL point, and where
    /// in that window's strip the point would insert a tab. Decorations
    /// make outer_position a few pixels off from the inner origin; the use
    /// (a drop target the width of a tab) forgives it.
    fn win_at(&self, gx: f32, gy: f32) -> Option<(usize, usize)> {
        for (i, w) in self.wins.iter().enumerate() {
            let Some(win) = w.window.as_ref() else { continue };
            let Ok(at) = win.outer_position() else { continue };
            let size = win.outer_size();
            let (x0, y0) = (at.x as f32, at.y as f32);
            if gx < x0 || gx >= x0 + size.width as f32 || gy < y0 || gy >= y0 + size.height as f32 {
                continue;
            }
            let lx = gx - x0;
            let mut insert = w.tabs.len();
            for (j, &(_, x1)) in w.tab_hits.iter().enumerate() {
                if lx < x1 {
                    insert = j;
                    break;
                }
            }
            return Some((i, insert));
        }
        None
    }

    fn drain_session_requests(&mut self, event_loop: &ActiveEventLoop) {
        let mut opened = false;
        while let Ok(req) = self.session_rx.try_recv() {
            match req {
                session::Request::OpenTab { window, cwd, argv } => {
                    // The number is the requesting shell's window; if it
                    // still exists the tab goes exactly there. A stale or
                    // absent number means "wherever the user is looking",
                    // which is already `cur`.
                    if window >= 1 && (window as usize) <= self.wins.len() {
                        self.cur = window as usize - 1;
                    }
                    // Trust but verify the directory: the caller may have
                    // raced its own rmdir; a vanished cwd means "who cares",
                    // not "fail".
                    let cwd = cwd.map(std::path::PathBuf::from).filter(|p| p.is_dir());
                    let cmd = if argv.is_empty() {
                        None
                    } else {
                        Some((argv[0].clone(), argv[1..].to_vec()))
                    };
                    // The remote could name a program that does not exist,
                    // and spawning one would take the whole session down
                    // with the panic. Refusing the request is the polite
                    // half of the deal; the log line is the other.
                    if let Some((program, _)) = cmd.as_ref() {
                        if !command_exists(program) {
                            log::warn!("session: refusing unknown command {program:?}");
                            continue;
                        }
                    }
                    self.add_tab_with(cwd, cmd);
                    opened = true;
                }
                session::Request::NewWindow { cwd, argv } => {
                    let cmd = if argv.is_empty() {
                        None
                    } else {
                        Some((argv[0].clone(), argv[1..].to_vec()))
                    };
                    if let Some((program, _)) = cmd.as_ref() {
                        if !command_exists(program) {
                            log::warn!("session: refusing unknown command {program:?}");
                            continue;
                        }
                    }
                    let cwd = cwd.map(std::path::PathBuf::from).filter(|p| p.is_dir());
                    log::info!("session: opened a remote window ({} windows now)", self.wins.len() + 1);
                    self.make_window(event_loop, cmd, cwd, true);
                    self.cur = self.wins.len() - 1;
                    opened = true;
                }
                session::Request::Raise => opened = true,
                session::Request::LastOutput { window, n, copy, respond } => {
                    // Fold any marks still in the ring first: the shell
                    // that typed `mypixels last` emitted its D mark
                    // moments ago, and that command is exactly what the
                    // user is asking about.
                    let text = self.last_output_text(window, n);
                    if copy && !text.is_empty() {
                        self.copy_text(clipboard::Kind::Clipboard, text.clone());
                    }
                    let _ = respond.send(text);
                }
            }
        }
        // The MCP endpoint's queries take the same channel shape as the
        // session bus's: answer on the main thread, reply on the oneshot.
        while let Ok(q) = self.mcp_rx.try_recv() {
            match q {
                mcp::Query::ListWindows { respond } => {
                    let v = serde_json::json!([
                        self.wins.iter().enumerate().map(|(i, w)| serde_json::json!({
                            "window": i + 1,
                            "active_tab": w.active + 1,
                            "tabs": w.tabs.iter().enumerate().map(|(j, t)| serde_json::json!({
                                "tab": j + 1,
                                "title": t.display_title(),
                                "cwd": t.cwd_raw().map(|p| p.to_string_lossy().into_owned()),
                            })).collect::<Vec<_>>()
                        })).collect::<Vec<_>>()
                    ]);
                    let _ = respond.send(v);
                }
                mcp::Query::LastOutput { window, n, respond } => {
                    let _ = respond.send(self.last_output_text(window, n));
                }
            }
        }
        if opened {
            if let Some(w) = &self.wins[self.cur].window {
                // The compositor owns focus; an attention request is how a
                // polite app says "over here" without stealing the keyboard.
                w.request_user_attention(Some(winit::window::UserAttentionType::Critical));
            }
            log::info!("session: opened a remote tab (tabs={})", self.wins[self.cur].tabs.len());
        }
    }

    fn listener(&self) -> SpikeListener {
        SpikeListener {
            window: Arc::clone(&self.window_slot),
            tab_id: 0,
            title: Arc::new(Mutex::new(String::new())),
            exited: Arc::clone(&self.exited),
            last_activity: Arc::clone(&self.last_activity),
            bell: Arc::clone(&self.bell),
            bells: Arc::clone(&self.bell_pings),
            sems: Arc::clone(&self.sems_ring),
            apcs: Arc::clone(&self.apcs_ring),
            pty_writes: Arc::clone(&self.pty_writes_ring),
            resets: Arc::clone(&self.resets_ring),
            cell_sizes: Arc::clone(&self.cell_sizes_ring),
        }
    }

    /// Remember a tab's death in the reopen ring (bounded, oldest first out).
    /// Takes the already-collected entry, not the tab: callers are holding a
    /// borrow of the tab list that remembering must not fight with.
    fn remember_closed(&mut self, entry: (String, Option<std::path::PathBuf>)) {
        self.closed_ring.push_back(entry);
        while self.closed_ring.len() > 8 {
            self.closed_ring.pop_front();
        }
    }

    fn close_tab_at(&mut self, idx: usize, event_loop: &ActiveEventLoop) {
        if idx >= self.wins[self.cur].tabs.len() {
            return;
        }
        let dying_id = self.wins[self.cur].tabs[idx].id;
        self.sem_blocks.remove(&dying_id);
        self.kitty.remove(&dying_id);
        self.forget_gpu_images(dying_id);
        self.remember_closed(tab_death(&self.wins[self.cur].tabs[idx]));
        // Stable ids: find the id of the tab being removed
        self.wins[self.cur].tabs.remove(idx);
        if self.wins[self.cur].tabs.is_empty() {
            // The last tab closes its WINDOW, not the application: a second
            // window must survive closing all of the first one's tabs.
            // (close_window ends the app when this was the last window.)
            self.close_window(self.cur, event_loop);
            return;
        }
        if self.wins[self.cur].active >= self.wins[self.cur].tabs.len() {
            self.wins[self.cur].active = self.wins[self.cur].tabs.len() - 1;
        } else if self.wins[self.cur].active > idx {
            self.wins[self.cur].active -= 1;
        }
    }

    /// Whether the tab strip is currently shown (manual hide flag and
    /// `hide_single_tab` both apply).
    fn show_tab_strip(&self) -> bool {
        !self.wins[self.cur].tablist_hidden && !(self.config.hide_single_tab && self.wins[self.cur].tabs.len() <= 1)
    }

    /// Resize terminal grid of all tabs to fit the current surface size.
    fn sync_term_size(&mut self) {
        let (w, h) = match self.wins[self.cur].gpu.as_ref() {
            Some(g) => (g.config.width, g.config.height),
            None => return,
        };
        if w == 0 || h == 0 {
            return;
        }
        let strip_h = if self.show_tab_strip() { self.tab_h } else { 0.0 };
        let term_h = (h as f32 - strip_h) as usize;
        let (mut ncols, mut nrows) = (
            (w as f32 / self.cell_w) as usize,
            (term_h as f32 / self.cell_h) as usize,
        );
        // Benchmark hook: ask for a grid larger than the window allows, to
        // separate grid-walk and shaping cost from rasterisation when the test
        // display is software only.
        if let Ok(g) = std::env::var("MYPIXELS_GRID") {
            if let Some((c, r)) = g.split_once('x') {
                ncols = c.parse().unwrap_or(ncols);
                nrows = r.parse().unwrap_or(nrows);
            }
        }
        if (ncols, nrows) != (self.wins[self.cur].cols, self.wins[self.cur].rows) {
            eprintln!("term resize: {}x{} -> {}x{}", self.wins[self.cur].cols, self.wins[self.cur].rows, ncols, nrows);
            self.wins[self.cur].cols = ncols;
            self.wins[self.cur].rows = nrows;
            self.wins[self.cur].dims.cols = ncols;
            self.wins[self.cur].dims.rows = nrows;
            let window_size = WindowSize {
                num_lines: nrows as u16,
                num_cols: ncols as u16,
                cell_width: self.cell_w as u16,
                cell_height: self.cell_h as u16,
            };
            for tab in &self.wins[self.cur].tabs {
                tab.term.lock().resize(self.wins[self.cur].dims);
                tab.term.lock().set_cell_pixels(self.cell_w as u32, self.cell_h as u32);
                let _ = tab.sender.send(Msg::Resize(window_size));
            }
        }
    }

    /// Re-apply everything a config reload can change. Colors, palette, tab
    /// colors, bindings and tablist visibility are read from `self.config` at
    /// draw time, so swapping the struct (done by the caller) covers them. We
    /// only need to recompute the values copied into App at startup: fg/bg and,
    /// when the font changed, the cell metrics + glyph pools.
    fn apply_config(&mut self, font_changed: bool) {
        self.fg = self.config.foreground_color.to_f32();
        self.bg = crate::config::terminal_bg(&self.config).to_f32();
        if let Some(gpu) = &mut self.wins[self.cur].gpu {
            gpu.slot_sig.fill(0);
            gpu.prev_sigs.fill(0);
        }
        if font_changed {
            self.apply_fonts();
        }
    }

    fn reload_config(&mut self) {
        match Config::try_load() {
            Ok(config) => {
                let font_changed = config.term_font != self.config.term_font || config.tablist_font != self.config.tablist_font;
                self.config = config;
                self.apply_config(font_changed);
                if let Some(watcher) = &self.theme_watcher { watcher.set_paths(self.config.theme_files.clone()); }
                if let Some(window) = &self.wins[self.cur].window { window.request_redraw(); }
                log::info!("configuration and colors reloaded");
            }
            Err(e) => self.show_error(format!("Theme/config reload: {e}")),
        }
    }

    fn check_theme_files(&mut self) {
        if self.theme_watcher.as_ref().is_some_and(|w| w.take_changed()) { self.reload_config(); }
    }

    /// Rebuild the terminal font from the live state (`term_family` +
    /// `term_pt`): attrs, cell metrics, the term glyph pool, and the grid.
    /// The base Metrics are baked into each pooled Buffer, so a change needs
    /// new buffers. Everything comes out in physical pixels: shaping happens
    /// at the size the glyphs are actually rasterised at, so HiDPI screens
    /// get the rounded cell of their real size, not a 1x cell doubled.
    fn rebuild_term_font(&mut self) {
        let font_px = self.term_pt * 96.0 / 72.0 * self.scale;
        self.term_metrics =
            fonts::line_metrics(&mut self.font_system, self.term_family, self.term_weight, self.term_slant)
                .unwrap_or(fonts::LineMetrics::DEFAULT);
        let term_lh = self.term_metrics.height_at(font_px);
        self.mono_attrs = {
            let db = self.font_system.db();
            fonts::pin_face(
                db,
                Attrs::new(),
                self.term_family,
                self.term_weight,
                self.term_slant,
            )
        };

        let mut measure =
            Buffer::new(&mut self.font_system, Metrics::new(font_px, term_lh));
        measure.set_size(Some(800.0), None);
        measure.set_text("MMMM", &self.mono_attrs, Shaping::Basic, None);
        measure.shape_until_scroll(&mut self.font_system, false);
        let shaped = measure.lines[0].shape(&mut self.font_system, 8);
        let mut total = 0.0;
        for span in &shaped.spans {
            for word in &span.words {
                total += word.width(font_px);
            }
        }
        let cell_w = (total / 4.0).round().max(1.0);
        let cell_h = term_lh.round().max(1.0);
        self.cell_w = cell_w;
        self.cell_h = cell_h;

        if let Some(gpu) = &mut self.wins[self.cur].gpu {
            gpu.pool = (0..MAX_RUNS)
                .map(|_| Buffer::new(&mut self.font_system, Metrics::new(font_px, term_lh)))
                .collect();
            gpu.prompt_buf =
                Buffer::new(&mut self.font_system, Metrics::new(font_px, term_lh));
            // New buffers hold nothing, so every cached signature is a lie.
            gpu.slot_sig = vec![0u64; gpu.pool.len()];
            gpu.prev_sigs.fill(0);
        }
        // Cell size changed -> the grid that fits the surface changes.
        self.sync_term_size();
        // The cell size is global, so refresh every tab's term - not just the
        // active window's - so a native-sized Kitty image in any window is
        // measured in the current cell, not a stale one.
        for w in self.wins.iter() {
            for t in &w.tabs {
                t.term.lock().set_cell_pixels(self.cell_w as u32, self.cell_h as u32);
            }
        }
        // Font routing depends on the terminal font, so rebuild it with it.
        *self.coverage.borrow_mut() = Some(fonts::Coverage::new(
            &self.font_system,
            &self.mono_attrs,
            &[self.term_family.to_string()],
        ));
        log::info!(
            "term font: {} {:.0}pt; cell = {:.2} x {:.2}",
            self.term_family, self.term_pt, cell_w, cell_h
        );
    }

    /// Attrs for a single codepoint: the terminal font, or whichever installed
    /// face actually covers the glyph when the terminal font does not.
    ///
    /// Built on demand because the startup path sets the font without going
    /// through `rebuild_term_font`; the font-rebuild path replaces it so a font
    /// change cannot leave a stale base behind.
    fn cell_attrs(&self, ch: char, flags: Flags) -> &'static Attrs<'static> {
        let mut cov = self.coverage.borrow_mut();
        let cov = cov.get_or_insert_with(|| {
            fonts::Coverage::new(
                &self.font_system,
                &self.mono_attrs,
                &[self.term_family.to_string()],
            )
        });
        cov.styled_attrs(self.font_system.db(), ch, flags.contains(Flags::BOLD), flags.contains(Flags::ITALIC))
    }

    /// Resolve the configured tab face for egui and refresh strip metrics.
    fn rebuild_tab_font(&mut self) {
        let (tab_spec, tab_pt) = config::parse_font(&self.config.tablist_font);
        let tab_font_px = tab_pt * 96.0 / 72.0 * self.scale;
        let tab = fonts::resolve_font(self.font_system.db(), &tab_spec);
        let tab_family: &'static str = Box::leak(tab.family.into_boxed_str());
        self.tab_face_key = egui_ui::preview_family_name(tab_family, tab.weight, tab.slant);
        self.tab_metrics =
            fonts::line_metrics(&mut self.font_system, tab_family, tab.weight, tab.slant)
                .unwrap_or(fonts::LineMetrics::DEFAULT);
        let tab_lh = self.tab_metrics.height_at(tab_font_px);
        self.tab_attrs = {
            let db = self.font_system.db();
            fonts::pin_face(db, Attrs::new(), tab_family, tab.weight, tab.slant)
        };

        let mut tmeasure =
            Buffer::new(&mut self.font_system, Metrics::new(tab_font_px, tab_lh));
        tmeasure.set_size(Some(800.0), None);
        tmeasure.set_text("MMMM", &self.tab_attrs, Shaping::Basic, None);
        tmeasure.shape_until_scroll(&mut self.font_system, false);
        let tshaped = tmeasure.lines[0].shape(&mut self.font_system, 8);
        let mut ttotal = 0.0;
        for span in &tshaped.spans {
            for word in &span.words {
                ttotal += word.width(tab_font_px);
            }
        }
        let tab_char_w = (ttotal / 4.0).max(1.0);
        self.tab_char_w = tab_char_w;
        self.tab_text_h = tab_lh;
        // Whole pixels again, for the same reason as the cell grid.
        self.tab_h = ui_style::tab_height(tab_lh, self.scale);

        self.sync_term_size();
        log::info!(
            "tab font: {} {:.0}pt; text {:.0}, strip {:.0}",
            tab_family, tab_pt, self.tab_text_h, self.tab_h
        );
    }

    /// Full font re-apply from config (used by config_reload). Resets the live
    /// term size to the config value, then rebuilds both fonts.
    fn apply_fonts(&mut self) {
        let (term_spec, term_pt) = config::parse_font(&self.config.term_font);
        let term = fonts::resolve_font(self.font_system.db(), &term_spec);
        self.term_family = Box::leak(term.family.into_boxed_str());
        self.term_weight = term.weight;
        self.term_slant = term.slant;
        self.term_pt = term_pt;
        self.rebuild_term_font();
        self.rebuild_tab_font();
    }

    /// Change the window scale factor. Every pixel metric is derived from
    /// point sizes, so rescaling is just re-running the two rebuilds, which
    /// also re-fit the grid to the surface. The compositor keeps the window's
    /// *physical* size across a scale change, so the grid shrinks to the
    /// bigger cells rather than the window growing.
    fn set_scale(&mut self, s: f32) {
        if s <= 0.0 || (s - self.scale).abs() < f32::EPSILON {
            return;
        }
        self.scale = s;
        self.rebuild_term_font();
        self.rebuild_tab_font();
    }

    /// enlarge_font / shrink_font: 1pt steps, unbounded up, floor of 1pt down
    /// (matching the Python zoom()).
    fn zoom_term(&mut self, zoom_in: bool) {
        if zoom_in {
            self.term_pt += 1.0;
        } else if self.term_pt > 1.0 {
            self.term_pt -= 1.0;
        } else {
            return;
        }
        self.rebuild_term_font();
    }

    /// The right-click menu, as (label, action, enabled) rows plus the
    /// indices where separators go. Actions are `do_action` names, so the
    /// menu can never offer something no binding could do.
    fn menu_rows(&self) -> (Vec<(String, String, bool)>, Vec<usize>) {
        let w = &self.wins[self.cur];
        let has_tab = w.active < w.tabs.len();
        let has_sel = w
            .tabs
            .get(w.active)
            .is_some_and(|t| t.term.lock().selection_to_string().is_some());
        let has_image = w.tabs.get(w.active).is_some_and(|t| {
            self
                .kitty
                .get(&t.id)
                .is_some_and(|k| k.placements().iter().any(|p| k.image(p.image_id).is_some()))
        });
        let mut items: Vec<(String, String, bool)> = Vec::new();
        let mut seps: Vec<usize> = Vec::new();
        let mut add = |label: &str, action: &str, enabled: bool, sep: bool| {
            if sep {
                seps.push(items.len());
            }
            items.push((label.to_string(), action.to_string(), enabled));
        };
        add("Copy", "copy", has_sel, false);
        add("Paste", "paste", true, false);
        add("Paste primary", "paste_primary", true, false);
        add("Select all", "select_all", has_tab, true);
        add("Copy as image", "copy_as_image", has_tab, true);
        add("Copy last image", "copy_last_image", has_image, false);
        add("Copy last output", "copy_last_output", has_tab, false);
        add("Command history", "show_history", has_tab, false);
        add("New tab", "new_tab", true, true);
        add("Rename tab", "rename_tab", has_tab, false);
        add("Close tab", "close_tab", has_tab, false);
        add("New window", "new_window", true, true);
        add("Send tab to new window", "send_tab_new_window", has_tab, false);
        add("Zoom in", "enlarge_font", has_tab, true);
        add("Zoom out", "shrink_font", has_tab, false);
        add("Reset zoom", "reset_font", has_tab, false);
        add("Settings...", "show_config", true, true);
        (items, seps)
    }

    fn build_frame(&self, tab: &Tab, show_cursor: bool) -> FrameData {
        let term = tab.term.lock();
        let content = term.renderable_content();
        let palette = Palette::from_colors(content.colors, &self.config.palette);
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        let resolve = |color: &Color| palette.resolve_with(color, self.fg, self.bg, self.config.cursor_color.to_f32());
        let fg = resolve(&Color::Named(NamedColor::Foreground));
        let bg = resolve(&Color::Named(NamedColor::Background));
        // The cursor is the one element the user must never have to hunt
        // for, whatever palette the theme carries: boost it halfway to
        // white. A theme that already chose a bright cursor loses nothing
        // (the mix flattens as it approaches 1.0), a dark one gains a lot.
        let cursor_color = {
            let c = resolve(&Color::Named(NamedColor::Cursor));
            c.map(|v| v + (1.0 - v) * 0.5)
        };
        let rows = self.wins[self.cur].rows;
        let cols = self.wins[self.cur].cols;

        let mut runs: Vec<Run> = Vec::new();
        let mut cur: Option<Run> = None;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + content.display_offset as i32;
            if row < 0 { continue; }
            let row = row as usize;
            if row >= rows {
                break;
            }
            let col = indexed.point.column.0;
            if col >= cols {
                continue;
            }
            let cell = indexed.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            // `put_tab` stores the tab byte in the cell; it is invisible and
            // shapes to a .notdef box, so it draws as a plain space.
            let c = if cell.c.is_control() { ' ' } else { cell.c };

            let mut fg = resolve(&cell.fg);
            let mut bg = resolve(&cell.bg);
            if cell.flags.contains(Flags::INVERSE) { std::mem::swap(&mut fg, &mut bg); }
            if cell.flags.contains(Flags::DIM) { fg = fg.map(|v| v * 0.66); }
            let attrs = self.cell_attrs(c, cell.flags);
            let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };

            // Continue the current run in place. Building a `String` per cell is
            // ~80k allocations a frame at 4K, and a debug build feels every one.
            if let Some(r) = cur.as_mut() {
                if r.row == row as u32
                    && r.col + r.width == col as u32
                    && r.fg == fg
                    && r.bg == bg
                    && std::ptr::eq(r.attrs, attrs)
                {
                    r.cells.push((r.text.len(), r.width));
                    r.width += width;
                    r.text.push(c);
                    if let Some(zw) = cell.zerowidth() {
                        r.text.extend(zw.iter().copied());
                    }
                    continue;
                }
            }
            if let Some(r) = cur.take() {
                push_run(&mut runs, r, fg, bg);
            }
            let mut glyph = String::with_capacity(4);
            glyph.push(c);
            if let Some(zw) = cell.zerowidth() {
                glyph.extend(zw.iter());
            }
            cur = Some(Run {
                row: row as u32,
                col: col as u32,
                width,
                fg,
                bg,
                text: glyph,
                cells: vec![(0, 0)],
                attrs,
            });
        }
        if let Some(r) = cur.take() {
            push_run(&mut runs, r, fg, bg);
        }

        let cursor = if !show_cursor {
            None
        } else {
            match content.cursor.shape {
                alacritty_terminal::vte::ansi::CursorShape::Hidden => None,
                _ if content.cursor.point.line.0 + (content.display_offset as i32) < self.wins[self.cur].rows as i32 => Some((
                    content.cursor.point.column.0 as u32,
                    (content.cursor.point.line.0 + content.display_offset as i32) as u32,
                )),
                _ => None,
            }
        };

        FrameData { runs, cursor, fg, bg, cursor_color }
    }

    /// Which way (and how fast) a selection drag at pointer height `py`
    /// should scroll. `top`/`bottom` bound the text grid. Inside a band at
    /// an edge, the pull is answered: the deeper toward (and past) the edge
    /// the pointer is pushed, the more lines travel per tick -- grazing the
    /// band drifts a line, pinning the pointer at the window's border
    /// scrolls almost a screen. The pointer cannot leave the window, so
    /// how far INTO the edge you drove it is the only honest measure of
    /// how hard the drag is; depth is the speed. Positive direction = up
    /// into history, the wheel's own sign.
    fn autoscroll_tick(py: f32, top: f32, bottom: f32, cell: f32) -> Option<(i32, i32)> {
        if bottom <= top || cell <= 0.0 {
            return None;
        }
        // A band taller than the grid would scroll on a pointer parked in
        // the middle of a tiny window.
        let band = (cell * 2.2).min((bottom - top) / 3.0);
        let cap = 32;
        if py < top + band {
            let depth = (top + band - py).max(0.0);
            Some((1, ((depth / cell).ceil() as i32).clamp(1, cap)))
        } else if py > bottom - band {
            let depth = (py - (bottom - band)).max(0.0);
            Some((-1, ((depth / cell).ceil() as i32).clamp(1, cap)))
        } else {
            None
        }
    }

    /// Render the terminal and, while Alt is held, the egui tab switcher.
    /// Builds a frame per tab (before the mutable gpu borrow), then hands
    /// off to the gpu pass.
    fn draw(&mut self, window: &Window) {
        if self.wins[self.cur].notice.as_ref().is_some_and(|(_, at)| at.elapsed().as_secs() >= 4) { self.wins[self.cur].notice = None; }
        // Selection-drag autoscroll: dragging the selection edge into the
        // band at either grid edge scrolls that way, and dragging PAST the
        // edge scrolls faster -- one extra line per tick for every cell
        // height of over-drag. The signs match the wheel: Delta(+) travels
        // up into history, Delta(-) back down.
        {
            let (px, py) = self.wins[self.cur].pointer;
            let strip_h = if self.show_tab_strip() { self.tab_h } else { 0.0 };
            let bottom = strip_h + self.wins[self.cur].rows as f32 * self.cell_h;
            let dir = if self.wins[self.cur].drag.is_some() && !self.wins[self.cur].tabs.is_empty() {
                Self::autoscroll_tick(py, strip_h, bottom, self.cell_h)
            } else {
                None
            };
            if let Some((d, lines)) = dir {
                if self.wins[self.cur].autoscroll_next.map_or(true, |t| Instant::now() >= t) {
                    self.wins[self.cur].autoscroll_next = Some(Instant::now() + std::time::Duration::from_millis(50));
                    let tab = &self.wins[self.cur].tabs[self.wins[self.cur].active];
                    tab.scroll(alacritty_terminal::grid::Scroll::Delta(d * lines));
                    // Same buffer-line transform as `cell_at`, applied to
                    // the edge row of the post-scroll view.
                    let col = ((px / self.cell_w).floor() as usize).min(self.wins[self.cur].cols.saturating_sub(1));
                    let within = px / self.cell_w - (px / self.cell_w).floor();
                    let side = if within < 0.5 { Side::Left } else { Side::Right };
                    let vis_row = if d > 0 { 0 } else { self.wins[self.cur].rows as i32 - 1 };
                    let off = tab.term.lock().grid().display_offset() as i32;
                    tab.update_selection(
                        Point { line: Line(vis_row - off), column: Column(col) },
                        side,
                    );
                    window.request_redraw();
                }
            } else {
                self.wins[self.cur].autoscroll_next = None;
            }
        }
        let t_start = Instant::now();
        let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) else {
            return;
        };
        // The active tab is "focused" iff the window has input focus
        // (covers tab switching, which produces no Focused event).
        {
            let active = &self.wins[self.cur].tabs[self.wins[self.cur].active];
            active.term.lock().is_focused = window.has_focus();
        }
        let blink = self.cursor_blink.load(Ordering::SeqCst);
        // The overview is an overlay on the live terminal, so the terminal is
        // still painted underneath it -- that is what the backdrop dims.
        // Every tab's frame is built first (thumbnails are live), before the
        // borrow of `tab` ends.
        // Visual bell: a brief lift of the background, VTE's built-in bell
        // with the sound removed. Run colors resolve from `self.bg` at build
        // time, so lifting it for the duration of the builds flashes the
        // whole terminal -- glyphs included -- not just the gaps between
        // runs. Frames re-request each other while lit.
        let bell_k = self.bell_flash();
        let saved_bg = self.bg;
        if bell_k > 0.0 {
            self.bg = bell_mix(self.bg, bell_k);
            window.request_redraw();
        }
        // "Copy as image" outside the switcher: no per-tab frames exist
        // then, so the shot builds its own (one frame's cost, once).
        let shot_frame: Option<FrameData> = if self.wins[self.cur].shot
            && self.wins[self.cur].overview.is_none()
        {
            self.wins[self.cur]
                .tabs
                .get(self.wins[self.cur].active)
                .map(|t| self.build_frame(t, false))
        } else {
            None
        };
        let ov_frames: Vec<FrameData> = if self.wins[self.cur].overview.is_some() {
            self.wins[self.cur].tabs.iter().map(|t| self.build_frame(t, true)).collect()
        } else {
            Vec::new()
        };
        let frame = self.build_frame(tab, blink);
        self.bg = saved_bg;
        let t_built = t_start.elapsed();
        let show_strip = self.show_tab_strip();
        let (w, h) = match &self.wins[self.cur].gpu {
            Some(g) => (g.config.width, g.config.height),
            None => return,
        };
        if w == 0 || h == 0 {
            return;
        }
        // egui (configuration window): run the UI before the GPU is moved
        // out of `self`, so applying a changed config can touch it.
        // What the drawing caches depend on; the rest of the config is read
        // live from `self.config` every frame.
        let old_text_state = (
            self.config.term_font.clone(),
            self.config.tablist_font.clone(),
            self.config.foreground_color,
            crate::config::terminal_bg(&self.config),
            self.config.col,
        );
        // The UI wears the terminal's EFFECTIVE colours (theme, palette and
        // overrides resolved upstream - not the raw config fields, which a
        // themed session bypasses entirely). Applied every frame: live
        // theme changes edit the palette under a running UI, and egui
        // reads visuals at layout time, not paint time.
        if let Some(e) = self.wins[self.cur].egui.as_ref() {
            e.ctx.set_visuals(ui_style::theme(&self.config, self.bg, self.fg));
        }
        let tab_faces: Vec<tab_bar::TabFace> = if show_strip {
            self.wins[self.cur].tabs.iter().enumerate().map(|(i, tab)| tab_bar::TabFace {
                id: tab.id,
                title: tab_title(tab, i == self.wins[self.cur].active, self.config.tab_title_max_length,
                    self.config.tab_title_short_current, &self.config.tab_shorten_str),
                tooltip: format!("{}\n{}", tab.display_title(), tab.current_dir().unwrap_or_default()),
                group: if tab.group < 8 { Some(ui_style::rgb_c32(self.config.col[tab.group])) } else { None },
                entangled: tab.entangled,
                attention: self.wins[self.cur].bell_pulse.filter(|(index, _)| *index == i)
                    .map_or(0.0, |(_, at)| (1.0 - at.elapsed().as_secs_f32() / 0.9).clamp(0.0, 1.0)),
            }).collect()
        } else { Vec::new() };
        let tab_font = egui::FontId::new(config::parse_font(&self.config.tablist_font).1 * 96.0 / 72.0,
            egui::FontFamily::Name(self.tab_face_key.clone().into()));
        let clock = (self.config.tab_prepend.as_deref() == Some("clock")).then(|| {
            let (hour, minute) = local_hhmm(); format!("{hour:02}:{minute:02}")
        });
        let mut strip_geometry = tab_bar::Geometry::default();
        let mut full_output: Option<egui::FullOutput> = None;
        // The switcher draws through egui; its state machine stays in the
        // app (Alt-hold open, navigation, Alt release dismiss). The older
        // hand-drawn overlay was deleted once the egui cut was confirmed:
        // two renderers of the same UI meant two of every bug.
        let sw_open = matches!(
                self.wins[self.cur].overview.map(|o| o.mode),
            Some(OverviewMode::Switcher));
        let sw_selected = self.wins[self.cur].active;
        let (sw_preview_w, sw_preview_h) = egui_ui::thumbnail_size(
            w as f32 / self.scale.max(1.0),
            w as f32 / (h as f32 - if show_strip { self.tab_h } else { 0.0 }).max(1.0),
            self.config.tab_overview_thumb_width,
            self.config.tab_overview_thumb_height,
        );
        let sw_cards: Vec<egui_ui::SwitcherCard> = if sw_open {
            self.wins[self.cur].tabs
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let title = t.display_title();
                    // Cards truncate within their header; the tooltip retains
                    // the full title and working directory.
                    let cwd_line = t.current_dir().filter(|c| c != &title);
                    let thumb = self.wins[self.cur].
                        sw_thumbs
                        .get(i)
                        .and_then(|t| t.as_ref())
                        .map(|t| (t.handle.clone(), t.size));
                    let status = self.sem_blocks.get(&t.id)
                        .and_then(|blocks| blocks.iter().rev().find(|b| b.is_history_command()))
                        .map(|b| if !b.done { egui_ui::SwitcherStatus::Running }
                            else if b.exit.is_some_and(|e| e != 0) { egui_ui::SwitcherStatus::Failed }
                            else if b.exit == Some(0) { egui_ui::SwitcherStatus::Passed }
                            else { egui_ui::SwitcherStatus::Completed })
                        .unwrap_or_default();
                    egui_ui::SwitcherCard {
                        status,
                        title,
                        active: i == self.wins[self.cur].active,
                        marker: t.entangled,
                        cwd: cwd_line,
                        thumb,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        let acc = self.config.accent_color.to_f32();
        let sw_accent = egui::Color32::from_rgb(
            (acc[0] * 255.0) as u8,
            (acc[1] * 255.0) as u8,
            (acc[2] * 255.0) as u8,
        );
        let sw_dim = egui_ui::dim_fill(
            &self.config,
            self.bg,
            (self.config.tab_overview_opacity.clamp(0.0, 1.0) * 255.0) as u8,
        );
        {
            let mut egui = self.wins[self.cur].egui.take();
            if let Some(egui) = egui.as_mut() {
                let mut raw = egui.state.take_egui_input(window);
                raw.events
                    .extend(std::mem::take(&mut self.wins[self.cur].fifo_egui_events));
                let ctx = egui.ctx.clone();
                // The history panel's rows are a snapshot of the pool,
                // rebuilt every frame while it is open: new commands show
                // up in the list with no extra plumbing, and a tab switch
                // swaps the list without the app noticing.
                if egui.ui_state.panel_is(egui_ui::RightPanel::History) {
                    if let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) {
                        egui.ui_state.history_rows = self.history_rows_for(tab.id);
                    }
                }
                egui.ui_state.panel_top = if show_strip { self.tab_h / ctx.pixels_per_point() } else { 0.0 };
                let full = ctx.run_ui(raw, |ui| {
                    if show_strip {
                        strip_geometry = tab_bar::show(ui, self.tab_h / ctx.pixels_per_point(), tab_font.clone(),
                            &tab_faces, self.wins[self.cur].active, clock.as_deref(), &self.config);
                    }
                    let mut config_open = egui.ui_state.panel_is(egui_ui::RightPanel::Settings);
                    let ev = egui_ui::config_window(
                        ui,
                        &mut config_open,
                        &mut self.config,
                        &mut egui.ui_state,
                        &egui.font_choices,
                    );
                    if !config_open {
                        egui.ui_state.close_panel(egui_ui::RightPanel::Settings);
                    }
                    egui.config_changed = ev.apply;
                    egui.save_requested = ev.save;
                    egui.revert_requested = ev.revert;
                    if sw_open {
                        egui_ui::switcher_screen(
                            ui,
                            true,
                            sw_selected,
                            sw_dim,
                            sw_accent,
                            &sw_cards,
                            egui::vec2(sw_preview_w.round(), sw_preview_h.round()),
                            &mut egui.switcher_pick,
                        );
                    }
                    egui_ui::rename_box(ui, &mut egui.ui_state);
                    egui_ui::move_pick_box(ui, &mut egui.ui_state);
                    egui_ui::ctx_menu(ui, &mut egui.ui_state);
                    if !sw_open && egui.ui_state.panel_is(egui_ui::RightPanel::History) {
                        // Status dots wear the palette's own green and red
                        // (the fixed pair only when no palette is set).
                        let pal = self.config.palette;
                        let ok = ui_style::rgb_c32(pal.map(|p| p[2]).unwrap_or(crate::config::Rgb(59, 193, 126)));
                        let err = ui_style::rgb_c32(pal.map(|p| p[1]).unwrap_or(crate::config::Rgb(239, 97, 109)));
                        let top = if show_strip { self.tab_h / ctx.pixels_per_point() } else { 0.0 };
                        egui_ui::history_panel(ui, &mut egui.ui_state, ok, err, top);
                    }
                    if let Some(r) = self.ripple.as_ref() {
                        let t = r.at.elapsed().as_secs_f32() / 0.18;
                        if t < 1.0 {
                            let rad = self.cell_h * (0.45 + 1.9 * t);
                            let a = ((1.0 - t) * 120.0) as u8;
                            let col = egui::Color32::from_rgba_unmultiplied(
                                (r.color[0] * 255.0) as u8,
                                (r.color[1] * 255.0) as u8,
                                (r.color[2] * 255.0) as u8,
                                a,
                            );
                            ui.painter().circle_stroke(
                                egui::pos2(r.x / self.scale, r.y / self.scale),
                                rad,
                                egui::Stroke::new(2.0, col),
                            );
                            // The pulse is a countdown: every live pulse
                            // schedules the frame that fades it.
                            window.request_redraw();
                        } else {
                            self.ripple = None;
                        }
                    }
                });
                egui.ui_state.release_passive_focus(&ctx, &full.platform_output);
                // New windows/areas need a sizing pass before they can
                // paint. Forward THIS pass's immediate repaint request;
                // checking only the previous pass can strand a new menu.
                if full.viewport_output.get(&egui::ViewportId::ROOT)
                    .is_some_and(|output| output.repaint_delay.is_zero())
                {
                    window.request_redraw();
                }
                // A panel/popup that closed this frame must surrender
                // keyboard focus; a vanished TextEdit does not do it alone,
                // and while egui holds focus every keystroke reads as
                // consumed and never reaches the terminal.
                if let Some(res) = egui.ui_state.rename_out.take() {
                    self.wins[self.cur].renaming = false;
                    egui.ui_state.rename_open = false;
                    if let Some(name) = res {
                        log::info!("rename_tab -> {:?}", name);
                        let cur_active = self.wins[self.cur].active;
                        if let Some(t) = self.wins[self.cur].tabs.get_mut(cur_active) {
                            t.name = if name.is_empty() { None } else { Some(name) };
                        }
                    }
                    window.request_redraw();
                }
                if let Some(pick) = egui.ui_state.move_out.take() {
                    egui.ui_state.move_open = false;
                    if let Some(t) = pick {
                        // Deferred: making a window needs the event loop,
                        // which the draw path does not have.
                        self.pending_move = Some(t);
                    }
                    window.request_redraw();
                }
                if let Some(pick) = egui.ui_state.menu_out.take() {
                    egui.ui_state.menu_open = false;
                    // History can remain open, so the combined popup-open
                    // flag has no falling edge to release the menu's focus.
                    egui_ui::release_focus(&ctx);
                    if let Some(action) = pick {
                        self.pending_action = Some(action);
                    }
                    window.request_redraw();
                }
                if let Some((seq, action)) = egui.ui_state.history_out.take() {
                    self.do_history_action(seq, &action);
                    window.request_redraw();
                }
                let popup_open = egui.ui_state.font_popup.as_ref().is_some_and(|p| p.open)
                    || egui.ui_state.rename_open
                    || egui.ui_state.move_open
                    || egui.ui_state.menu_open;
                egui_ui::release_focus_if_closed(&ctx, egui.ui_state.right_panel != egui_ui::RightPanel::Closed, popup_open, &mut egui.open_prev);
                full_output = Some(full);
            }
            self.wins[self.cur].egui = egui;
        }
        let scale = self.scale;
        self.wins[self.cur].tab_hits = strip_geometry.tabs.iter().map(|r| (r.left() * scale, r.right() * scale)).collect();
        self.wins[self.cur].add_hit = (strip_geometry.add.left() * scale, strip_geometry.add.right() * scale);
        self.wins[self.cur].tab_close_hits = strip_geometry.close.into_iter()
            .map(|r| egui::Rect::from_min_max(r.min * scale, r.max * scale)).collect();
        let (revert, changed, save) = self.wins[self.cur].
            egui
            .as_ref()
            .map(|e| (e.revert_requested, e.config_changed, e.save_requested))
            .unwrap_or_default();
        if revert {
            self.reload_config();
        } else if changed {
            // Re-apply only when a cached value moved. Opacity, switcher and
            // geometry fields are read live per frame, and forcing a full
            // re-shape on every slider tick would stutter the drag.
            let font_changed = old_text_state.0 != self.config.term_font
                || old_text_state.1 != self.config.tablist_font;
            let colors_changed = old_text_state.2 != self.config.foreground_color
                || old_text_state.3 != crate::config::terminal_bg(&self.config)
                || old_text_state.4 != self.config.col;
            if font_changed || colors_changed {
                self.apply_config(font_changed);
            }
        }
        if save {
            match self.config.save() {
                // Done on purpose: saving closes the panel, so the user
                // sees the result on the terminal, not in the dialog.
                Ok(()) => {
                    log::info!("configuration saved");
                    if let Some(e) = self.wins[self.cur].egui.as_mut() {
                        e.ui_state.close_panel(egui_ui::RightPanel::Settings);
                    }
                }
                Err(err) => self.show_error(err),
            }
        }
        if let Some(e) = self.wins[self.cur].egui.as_mut() {
            e.config_changed = false;
            e.save_requested = false;
            e.revert_requested = false;
            // A switcher card was clicked: activate that tab and dismiss.
            if let Some(i) = e.take_switcher_pick() {
                if i < self.wins[self.cur].tabs.len() {
                    self.wins[self.cur].active = i;
                }
                self.close_switcher();
            }
        }
        // Moved out of `self` so the drawing code below can keep borrowing
        // other `self` fields freely. Put back before every exit.
        let mut gpu = self.wins[self.cur].gpu.take().expect("gpu present, checked above");
        let wf = w as f32;
        let hf = h as f32;

        let strip_h = if show_strip { self.tab_h } else { 0.0 };

        // URLs: rescan the visible grid and locate the pointer on them.
        // Done before the window split-borrow below, which is where the
        // underline's rect actually gets pushed.
        self.url_spans = match self.visual_cell(
            self.wins[self.cur].pointer.0,
            self.wins[self.cur].pointer.1,
        ) {
            Some((row, _)) => self.scan_row_cached(row),
            None => Vec::new(),
        };
        self.url_hover = {
            let (px, py) = self.wins[self.cur].pointer;
            self.visual_cell(px, py).and_then(|(row, col)| {
                self.url_spans
                    .iter()
                    .position(|s| s.row == row && col >= s.start && col < s.end)
            })
        };
        let url_underline: Option<(f32, f32, f32)> = match self.url_hover.and_then(|i| self.url_spans.get(i)) {
            Some(sp) => {
                let strip_h = if self.show_tab_strip() { self.tab_h } else { 0.0 };
                Some((
                    sp.start as f32 * self.cell_w,
                    strip_h + (sp.row + 1) as f32 * self.cell_h - (self.cell_h * 0.07).max(2.0),
                    (sp.end - sp.start) as f32 * self.cell_w,
                ))
            }
            None => None,
        };
        gpu.rects.clear();
        // Convert pixel coords to clip space. Clip is -1..1 on both axes with
        // +y pointing up, screen pixels have y pointing down, so flip y.
        let push_rect =
            |v: &mut Vec<gpu::RectVertex>, x: f32, y: f32, rw: f32, rh: f32, c: [f32; 4]| {
                v.extend_from_slice(&gpu::rect_vertices(x, y, rw, rh, c, 0.0, wf, hf));
            };

        // Window background
        // The terminal background is the only translucent layer, matching the
        // original: it sets VTE's background to alpha = 1 - transparency and
        // leaves the tab strip opaque. `term_transparency` is therefore an
        // amount of *transparency*, not of opacity -- 0.1 means 10% see-through.
        let transparency = effective_transparency(self.config.term_transparency);
        let bg_a = if gpu.alpha_ok && transparency > 0.0 {
            1.0 - transparency
        } else {
            1.0
        };
        static BG_LOG: std::sync::Once = std::sync::Once::new();
        BG_LOG.call_once(|| {
            log::info!(
                "background alpha {:.3} (alpha_ok {}, term_transparency {:.2} -> effective {:.2}, surface alpha {:?})",
                bg_a,
                gpu.alpha_ok,
                self.config.term_transparency,
                transparency,
                gpu.config.alpha_mode
            );
        });
        push_rect(&mut gpu.rects, 0.0, 0.0, w as f32, h as f32, [frame.bg[0], frame.bg[1], frame.bg[2], bg_a]);

        // --- Text areas (tab labels + terminal runs), one prepare() per frame ---
        let mut areas: Vec<TextArea> = Vec::new();
        let bounds = TextBounds { left: 0, top: 0, right: w as i32, bottom: h as i32 };

        // --- Terminal: cell backgrounds ---
        for run in &frame.runs {
            let same_bg = (run.bg[0] - frame.bg[0]).abs() < 0.004
                && (run.bg[1] - frame.bg[1]).abs() < 0.004
                && (run.bg[2] - frame.bg[2]).abs() < 0.004;
            if !same_bg {
                push_rect(
                    &mut gpu.rects,
                    run.col as f32 * self.cell_w,
                    strip_h + run.row as f32 * self.cell_h,
                    run.width as f32 * self.cell_w,
                    self.cell_h,
                    [run.bg[0], run.bg[1], run.bg[2], 1.0],
                );
            }
        }
        // Selection wash: over the cell backgrounds, under the cursor and the
        // glyphs, and translucent so coloured text stays readable underneath --
        // an opaque wash would hide exactly the text the user selected.
        for (row, left, right) in self.wins[self.cur].tabs[self.wins[self.cur].active].selection_rows() {
            push_rect(
                &mut gpu.rects,
                left as f32 * self.cell_w,
                strip_h + row as f32 * self.cell_h,
                (right - left) as f32 * self.cell_w,
                self.cell_h,
                // Adwaita's selection blue, at the alpha terminals like VTE use
                // for an overlay rather than a colour swap.
                { let c = self.config.selection_color.to_f32(); [c[0], c[1], c[2], 0.35] },
            );
        }
        if let Some((cx, cy)) = frame.cursor {
            {
                push_rect(
                    &mut gpu.rects,
                    cx as f32 * self.cell_w,
                    strip_h + cy as f32 * self.cell_h,
                    self.cell_w,
                    self.cell_h,
                    [frame.cursor_color[0], frame.cursor_color[1], frame.cursor_color[2], 0.55],
                );
            }
            if let Some((ux, uy, uw)) = url_underline {
                // One hair under the word: enough to say "this is a link
                // and you are on it", too quiet to compete with real
                // underlines the shell itself may print.
                push_rect(
                    &mut gpu.rects,
                    ux,
                    uy,
                    uw,
                    (self.cell_h * 0.07).max(2.0),
                    [frame.fg[0], frame.fg[1], frame.fg[2], 0.7],
                );
            }
            self.cursor_pt = Some((
                [
                    cx as f32 * self.cell_w + self.cell_w * 0.5,
                    strip_h + cy as f32 * self.cell_h + self.cell_h * 0.5,
                ],
                frame.cursor_color,
            ));
        } else {
            self.cursor_pt = None;
        }

        // --- Terminal text ---
        // SAFETY: pool indices are disjoint (run index < MAX_RUNS), the pool
        // never reallocates after init, and the derived shared references are
        // only used this frame, before any further pool writes. The switcher
        // draws through egui and never touches this pool.
        {
            let cache = shaping_cache_enabled();
            let fs = &mut self.font_system;
            let pool = &mut gpu.pool;
            for (i, run) in frame.runs.iter().take(MAX_RUNS).enumerate() {
                if self.wins[self.cur].notice.is_some() && run.row as usize == self.wins[self.cur].rows.saturating_sub(1) { continue; }
                let blank = run.text.chars().all(|c| c == ' ');
                let default_fg =
                    (run.fg[0] - frame.fg[0]).abs() + (run.fg[1] - frame.fg[1]).abs()
                        + (run.fg[2] - frame.fg[2]).abs()
                        < 0.01;
                if blank && default_fg {
                    continue;
                }
                let sig = run_sig(run, self.cell_w);
                if !(cache && gpu.slot_sig.get(i) == Some(&sig)) {
                    gpu.slot_sig[i] = sig;
                    let b = &mut pool[i];
                    fonts::shape_terminal_run(b, fs, &run.text, run.attrs, &run.cells, self.cell_w);
                }
                let buf: &Buffer = unsafe { &*(&pool[i] as *const Buffer) };
                areas.push(gpu::terminal_text_area(
                    buf,
                    run.col as f32 * self.cell_w,
                    strip_h + run.row as f32 * self.cell_h,
                    run.fg,
                    bounds,
                ));
            }
        }

        // --- Notice prompt (overlays the last terminal row; renaming moved
        // into egui and no longer paints over the grid) ---
        if self.wins[self.cur].notice.is_some() {
            let prompt_y = strip_h + self.wins[self.cur].rows.saturating_sub(1) as f32 * self.cell_h;
            push_rect(
                &mut gpu.rects,
                0.0,
                prompt_y,
                w as f32,
                self.cell_h,
                [self.bg[0], self.bg[1], self.bg[2], 1.0],
            );
            let prompt = format!("  {}", self.wins[self.cur].notice.as_ref().unwrap().0);
            {
                let fs = &mut self.font_system;
                let b = &mut gpu.prompt_buf;
                b.set_size(Some(w as f32), None);
                b.set_text(&prompt, &self.mono_attrs, Shaping::Basic, None);
                b.set_monospace_width(Some(self.cell_w));
                b.shape_until_scroll(fs, false);
                let ptr: *const Buffer = b;
                let buf: &Buffer = unsafe { &*ptr };
                areas.push(TextArea {
                    buffer: buf,
                    left: 0.0,
                    top: prompt_y,
                    scale: 1.0,
                    bounds,
                    default_color: glyphon::Color::rgb(
                        (self.fg[0] * 255.0) as u8,
                        (self.fg[1] * 255.0) as u8,
                        (self.fg[2] * 255.0) as u8,
                    ),
                    custom_glyphs: &[],
                });
            }
        }

        let t_text = t_start.elapsed();
        // Every tile shows a real render, and no tab is rebuilt every frame. A
        // preview is refreshed when its content changed *and* its own throttle has
        // expired -- 60ms for the tile you are on, 200ms for the rest -- with at
        // most two rebuilt per frame so eight busy tabs cannot turn one redraw into
        // a 60ms hitch. A throttled tile keeps showing its previous frame, which is
        // plenty for a thumbnail. A tab that has never been drawn ignores the cap,
        // so opening the overlay fills the whole grid in one go.
        // The egui switcher's card thumbnails read these offscreen preview
        // frames back as textures (readback_preview), so every open switcher
        // keeps them warm.
        let mut jobs: Vec<gpu::PreviewJob> = Vec::new();
        if sw_open {
            if let Some(ov) = &self.wins[self.cur].overview {
            let n = self.wins[self.cur].tabs.len();
            let sel = ov.selected.min(n.saturating_sub(1));
            gpu.ensure_previews(n, (w, h));
            let now = Instant::now();
            // Selected tab first, then every other tab exactly once, from a
            // rotating cursor so a backlog drains in order and nothing starves.
            let mut order: Vec<usize> = Vec::with_capacity(n);
            order.push(sel);
            for k in 0..n {
                let i = (gpu.prev_rr + k) % n;
                if i != sel {
                    order.push(i);
                }
            }
            gpu.prev_rr = (gpu.prev_rr + 1) % n.max(1);
            let mut built = 0usize;
            for i in order {
                let Some(fd) = ov_frames.get(i) else {
                    continue;
                };
                let sig = frame_sig(fd, self.cell_w);
                let held = gpu.prev_sigs.get(i).copied().unwrap_or(0);
                if held == sig {
                    continue;
                }
                let gate = if i == sel { 60 } else { 200 };
                let throttled = held != 0
                    && now.duration_since(gpu.prev_last[i])
                        < std::time::Duration::from_millis(gate);
                if throttled || (held != 0 && built >= 2) {
                    continue;
                }
                let metrics = gpu.pool[0].metrics();
                let buffers = &mut gpu.prev_bufs[i];
                let runs = &fd.runs[..fd.runs.len().min(MAX_RUNS)];
                buffers.resize_with(runs.len(), || Buffer::new(&mut self.font_system, metrics));
                for (buffer, run) in buffers.iter_mut().zip(runs) {
                    buffer.set_metrics(metrics);
                    fonts::shape_terminal_run(
                        buffer, &mut self.font_system, &run.text, run.attrs, &run.cells, self.cell_w,
                    );
                }
                gpu.set_prev_sig(i, sig);
                gpu.prev_last[i] = now;
                jobs.push(gpu::PreviewJob {
                    idx: i,
                    bounds,
                    text: runs.iter().map(|run| gpu::PreviewText {
                        left: run.col as f32 * self.cell_w,
                        top: strip_h + run.row as f32 * self.cell_h,
                        color: run.fg,
                    }).collect(),
                    rects: preview_rects(
                        fd,
                        &fd.bg,
                        &self.config.tablist_background.to_f32(),
                        &fd.cursor_color,
                        self.cell_w,
                        self.cell_h,
                        strip_h,
                        self.wins[self.cur].rows,
                        w,
                        h,
                    ),
                });
                built += 1;
            }
            }
        }
        // One-shot screenshot: shape the captured frame into the preview
        // slot and queue the offscreen render. The readback happens after
        // render_frame -- same textures, same pipeline the switcher uses,
        // just at native resolution and exactly once.
        let mut shot_at: Option<usize> = None;
        if self.wins[self.cur].shot {
            let a = self.wins[self.cur].active;
            if a < self.wins[self.cur].tabs.len() {
                gpu.ensure_previews(a + 1, (w, h));
                if let Some(fd) = ov_frames.get(a).or(shot_frame.as_ref()) {
                    let metrics = gpu.pool[0].metrics();
                    let buffers = &mut gpu.prev_bufs[a];
                    let runs = &fd.runs[..fd.runs.len().min(MAX_RUNS)];
                    buffers.resize_with(runs.len(), || Buffer::new(&mut self.font_system, metrics));
                    for (buffer, run) in buffers.iter_mut().zip(runs) {
                        buffer.set_metrics(metrics);
                        fonts::shape_terminal_run(
                            buffer, &mut self.font_system, &run.text, run.attrs, &run.cells, self.cell_w,
                        );
                    }
                    gpu.set_prev_sig(a, frame_sig(fd, self.cell_w));
                    jobs.push(gpu::PreviewJob {
                        idx: a,
                        bounds,
                        text: runs.iter().map(|run| gpu::PreviewText {
                            left: run.col as f32 * self.cell_w,
                            top: strip_h + run.row as f32 * self.cell_h,
                            color: run.fg,
                        }).collect(),
                        rects: preview_rects(
                            fd,
                            &fd.bg,
                            &self.config.tablist_background.to_f32(),
                            &fd.cursor_color,
                            self.cell_w,
                            self.cell_h,
                            strip_h,
                            self.wins[self.cur].rows,
                            w,
                            h,
                        ),
                    });
                    shot_at = Some(a);
                }
            } else {
                self.wins[self.cur].shot = false;
            }
        }
        // Kitty images for the active tab: placements anchored to absolute
        // logical lines, mapped through the tab's scroll position to window
        // pixels, clipped to the grid area (UV-split so a half-scrolled
        // image peels off the top realistically). Text paints afterwards,
        // so glyphs stay on top of whatever an image covers.
        gpu.images = {
            let mut draws = Vec::new();
            let a = self.wins[self.cur].active;
            if let Some(tab) = self.wins[self.cur].tabs.get(a) {
                let tab_id = tab.id as u32;
                if let Some(k) = self.kitty.get(&tab.id) {
                    let (hist, off) = {
                        let term = tab.term.lock();
                        (term.grid().history_size(), term.grid().display_offset())
                    };
                    let top_abs = hist.saturating_sub(off);
                    let vy0 = strip_h;
                    let vy1 = strip_h + self.wins[self.cur].rows as f32 * self.cell_h;
                    for pl in k.placements_sorted() {
                        let Some(img) = k.image(pl.image_id) else { continue };
                        let iw = pl
                            .cols
                            .map_or(img.width, |c| (c as f32 * self.cell_w).max(1.0) as u32);
                        let ih = pl
                            .rows
                            .map_or(img.height, |r| (r as f32 * self.cell_h).max(1.0) as u32);
                        let x0 = pl.col as f32 * self.cell_w + pl.x_off as f32;
                        let y0 = vy0
                            + (pl.anchor_line as f32 - top_abs as f32) * self.cell_h
                            + pl.y_off as f32;
                        let (cx0, cy0) = (x0.max(0.0), y0.max(vy0));
                        let (cx1, cy1) = ((x0 + iw as f32).min(w as f32), (y0 + ih as f32).min(vy1));
                        if cx1 <= cx0 || cy1 <= cy0 {
                            continue;
                        }
                        let key = (tab_id, pl.image_id, img.generation);
                        if !gpu.has_image(key) {
                            gpu.upload_image(key, &img.rgba, img.width, img.height);
                        }
                        draws.push(gpu::ImageQuad {
                            key,
                            dst: [cx0, cy0, cx1 - cx0, cy1 - cy0],
                            uv: [
                                (cx0 - x0) / iw as f32,
                                (cy0 - y0) / ih as f32,
                                (cx1 - x0) / iw as f32,
                                (cy1 - y0) / ih as f32,
                            ],
                        });
                    }
                }
            }
            draws
        };
        let egui_frame = match (full_output.as_mut(), self.wins[self.cur].egui.as_mut()) {
            (Some(full), Some(egui)) => {
                // Take the shapes out: `full` is behind a reference, and the
                // tessellated jobs supersede them (only the texture deltas
                // are still read from `full`).
                let paint_jobs = egui
                    .ctx
                    .tessellate(std::mem::take(&mut full.shapes), full.pixels_per_point);
                Some(egui_ui::EguiFrame {
                    renderer: &mut egui.renderer,
                    full,
                    paint_jobs,
                    screen: egui_wgpu::ScreenDescriptor {
                        size_in_pixels: [w, h],
                        pixels_per_point: self.scale,
                    },
                })
            },
            _ => None,
        };
        gpu.render_frame(&mut self.font_system, areas, jobs, window, egui_frame);

        // Finish the screenshot: read the offscreen frame (already rendered
        // this pass) at full resolution and hand a PNG to the clipboard.
        if let Some(a) = shot_at {
            self.wins[self.cur].shot = false;
            match gpu.readback_preview(a, 0, (w, h)) {
                Some(rgba) => {
                    let png = png_rgba(w, h, &rgba);
                    let png_len = png.len();
                    let result = match self.clipboard.as_ref() {
                        None => Err("no clipboard backend".to_string()),
                        Some(_) if png.is_empty() => Err("PNG encoding failed".to_string()),
                        Some(cb) => cb.set_image(clipboard::Kind::Clipboard, png),
                    };
                    match result {
                        Ok(()) => {
                            log::info!("copy as image: {w}x{h} png {png_len} bytes");
                            self.wins[self.cur].notice =
                                Some(("Screen copied as image".to_string(), Instant::now()));
                        }
                        Err(e) => {
                            log::warn!("copy as image failed: {e}");
                            self.show_error(format!("Copy as image: {e}"));
                        }
                    }
                }
                None => {
                    log::warn!("copy as image: readback failed");
                    self.show_error("Copy as image: capture failed".to_string());
                }
            }
        }

        // The egui switcher's live thumbnails: read the freshly rendered
        // preview textures back for a few tabs per frame (same 60/200ms
        // throttling and two-per-frame cap as the preview jobs) and upload
        // them to egui. The thumbnail takes the terminal area of the texture
        // at its own aspect, so a card shows the tab without letterboxing.
        // A card click may have closed the overlay earlier in this draw.
        // Skip readbacks without returning: the moved-out GPU must be restored.
        if let (true, Some(ov), Some(ctx)) = (
            sw_open,
            self.wins[self.cur].overview,
            self.wins[self.cur].egui.as_ref().map(|e| e.ctx.clone()),
        ) {
            let n = self.wins[self.cur].tabs.len();
            let sel = ov.selected.min(n.saturating_sub(1));
            let term_h = ((h as f32) - strip_h).max(1.0);
            // The point size (egui space) follows the same config values as
            // the hand-drawn overview: `tab_overview_thumb_width/height`,
            // capped so four fit across the window, aspect fitted.
            let (ptw, pth) = egui_ui::thumbnail_size(
                (w as f32) / self.scale.max(1.0),
                (w as f32) / term_h,
                self.config.tab_overview_thumb_width,
                self.config.tab_overview_thumb_height,
            );
            // The egui context runs at the display's pixels-per-point, so
            // read the preview back at full pixel density -- a texture at
            // point size would be upscaled and look soft.
            let tw = (ptw * self.scale).round().max(1.0) as u32;
            let th = (pth * self.scale).round().max(1.0) as u32;
            let mut order: Vec<usize> = vec![sel];
            for k in 0..n {
                let i = (self.wins[self.cur].thumb_rr + k) % n;
                if i != sel {
                    order.push(i);
                }
            }
            self.wins[self.cur].thumb_rr = (self.wins[self.cur].thumb_rr + 1).rem_euclid(n.max(1));
            let now = Instant::now();
            let mut done = 0;
            for i in order {
                if done >= 2 {
                    break;
                }
                // Nothing to read back until the jobs block has rendered this
                // tab (signature 0 = never), and an unchanged screen does not
                // need a new readback.
                let sig = gpu.prev_sigs.get(i).copied().unwrap_or(0);
                if sig == 0 {
                    continue;
                }
                let interval =
                    std::time::Duration::from_millis(if i == sel { 60 } else { 200 });
                let stale = self.wins[self.cur].
                    sw_thumbs
                    .get(i)
                    .and_then(|t| t.as_ref())
                    .is_none_or(|t| {
                        t.pixels != (tw, th)
                            || (t.sig != sig && now.duration_since(t.last) >= interval)
                    });
                if !stale {
                    continue;
                }
                let Some(rgba) = gpu.readback_preview(i, strip_h as u32, (tw, th)) else {
                    continue;
                };
                let handle = ctx.load_texture(
                    format!("switcher-preview-{i}"),
                    egui::ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], &rgba),
                    egui::TextureOptions::LINEAR,
                );
                if self.wins[self.cur].sw_thumbs.len() < n {
                    self.wins[self.cur].sw_thumbs.resize(n, None);
                }
                self.wins[self.cur].sw_thumbs[i] = Some(SwThumb {
                    handle,
                    // Point size for the layout (the texture is pixel-dense).
                    size: (ptw.round() as u32, pth.round() as u32),
                    pixels: (tw, th),
                    sig,
                    last: now,
                });
                done += 1;
            }
        }
        self.wins[self.cur].gpu = Some(gpu);

        if std::env::var("MYPIXELS_TRACE").is_ok() {
            let mut h: u64 = 0xcbf29ce484222325;
            for run in &frame.runs {
                for b in run.text.as_bytes() {
                    h ^= *b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
            eprintln!(
                "frame build {t_built:?} text {t_text:?} total {:?} grid {}x{} runs {} hash {h:x}",
                t_start.elapsed(),
                self.wins[self.cur].cols,
                self.wins[self.cur].rows,
                frame.runs.len(),
            );
        }
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    /// Test hook: drain key specs from the $MYPIXELS_KEYS reader thread and feed
    /// them through the same path as real key events.
    fn drain_test_keys(&mut self, event_loop: &ActiveEventLoop) {
        let rx = match self.key_rx.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        let mut specs = Vec::new();
        while let Ok(line) = rx.try_recv() {
            specs.push(line);
        }
        if specs.is_empty() {
            return;
        }
        for line in &specs {
            if line == "spy" {
                // Test hook: dump tab state
                let state: Vec<String> = self.wins[self.cur].
                    tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let title = t.display_title().chars().take(30).collect::<String>();
                        format!("{i}[:{}](g{}e{}){}", t.id, t.group, if t.entangled { 1 } else { 0 }, title)
                    })
                    .collect();
                let cursor = match self.wins[self.cur].tabs.get(self.wins[self.cur].active) {
                    Some(t) => {
                        let c = t.term.lock().renderable_content().cursor;
                        let focus = t.term.lock().is_focused;
                        format!("shape={:?} point=({},{}) focus={}", c.shape, c.point.column.0, c.point.line.0, focus)
                    },
                    None => "-".to_string(),
                };
                let surf = match self.wins[self.cur].gpu.as_ref() {
                    Some(g) => format!("{}x{}", g.config.width, g.config.height),
                    None => "-".to_string(),
                };
                let ren = if self.wins[self.cur].renaming {
                    format!(
                        "renaming={:?}",
                        self.wins[self.cur].egui.as_ref().map(|e| e.ui_state.rename_draft.clone())
                    )
                } else {
                    String::new()
                };
                log::info!("SPY active={} surf={} cursor=[{}] tabs=[{}] {}", self.wins[self.cur].active, surf, cursor, state.join(","), ren);
                continue;
            }
            if line == "switcher" {
                // Test hook: open the Alt-held switcher directly (bypass the
                // hold delay, which needs real ModifiersChanged events).
                if self.wins[self.cur].overview.map(|o| o.mode) != Some(OverviewMode::Switcher)
                    && !self.wins[self.cur].tabs.is_empty()
                    && !self.wins[self.cur].renaming
                {
                    self.wins[self.cur].overview = Some(OverviewState {
                        mode: OverviewMode::Switcher,
                        selected: self.wins[self.cur].active,
                        original: self.wins[self.cur].active,
                    });
                    if let Some(e) = self.wins[self.cur].egui.as_ref() {
                        egui_ui::release_focus(&e.ctx);
                    }
                    self.switcher_open.store(true, Ordering::SeqCst);
                }
                continue;
            }
            if line == "shot" {
                // Test hook: fire the context menu's "Copy as image".
                self.do_action("copy_as_image", event_loop);
                continue;
            }
            if line == "history" {
                // Test hook: toggle the command-history panel.
                self.do_action("show_history", event_loop);
                continue;
            }
            if line == "alt_up" {
                // Test hook: simulate Alt release (dismiss the switcher).
                *self.switcher_alt_at.lock().unwrap() = None;
                if self.switcher_open.load(Ordering::SeqCst) {
                    self.close_switcher();
                }
                continue;
            }
            // Test hooks: raw pty writes to the active tab. Composing a
            // shell command out of key specs (or uinput) is how a harness
            // races the compositor and the user's own keyboard; these
            // directives speak to the child directly. `line:` appends \n.
            // The FIFO itself only exists when MYPIXELS_KEYS is set.
            if line == "colors" {
                // Test hook: dump every OSC-set entry in the terminal's
                // color table. These override the config palette, so a
                // shell/rc that emits OSC 4/10/11 can silently repaint
                // the theme - this shows exactly who did what.
                let Some(t) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) else {
                    continue;
                };
                let term = t.term.lock();
                let content = term.renderable_content();
                let names = [
                    "fg", "bg", "cursor", "sel_fg", "sel_bg",
                ];
                let mut parts = Vec::new();
                for i in 0..256 {
                    if let Some(c) = &content.colors[i] {
                        parts.push(format!("{i}#{:02x}{:02x}{:02x}", c.r, c.g, c.b));
                    }
                }
                for (k, n) in (256..269).zip(names.into_iter().chain(std::iter::repeat("?"))) {
                    if let Some(c) = &content.colors[k] {
                        parts.push(format!("{k}{n}#{:02x}{:02x}{:02x}", c.r, c.g, c.b));
                    }
                }
                log::info!("COLORS [{}]", parts.join(" "));
                continue;
            }
            if let Some(rest) =
                line.strip_prefix("text:").or_else(|| line.strip_prefix("line:"))
            {
                let nl = line.starts_with("line:");
                if let Some(t) =
                    self.wins[self.cur].tabs.get(self.wins[self.cur].active)
                {
                    let mut bytes = rest.as_bytes().to_vec();
                    if nl {
                        bytes.push(b'\n');
                    }
                    t.write(&bytes);
                }
                continue;
            }
            if let Some((mods, key)) = parse_key_spec(line) {
                self.modifiers = mods;
                // Mirror the real path's egui-first routing: while an egui
                // widget owns the keyboard (rename box, font popup, config
                // fields), the spec goes to egui and never reaches the
                // terminal. winit's KeyEvent cannot be constructed from
                // outside winit (pub(crate) extras), so translate to egui
                // events directly -- the same things egui-winit would send.
                if !self.test_key_to_egui(&key, mods) {
                    self.handle_key(&key, None, event_loop);
                }
            }
        }
        if let Some(window) = self.wins[self.cur].window.as_ref() {
            window.request_redraw();
        }
    }

    /// The active terminal's mouse-reporting mode, or None when the terminal
    /// does not want the mouse. Shift is the bypass: while it is held the
    /// mouse always selects instead of reporting (the VTE/Alacritty rule).
    /// Feed a FIFO key spec to egui when egui holds the keyboard. Returns
    /// whether egui claimed it (so the caller skips the app's own handler).
    fn test_key_to_egui(&mut self, key: &Key, mods: ModifiersState) -> bool {
        if self.wins[self.cur].egui.is_none() {
            return false;
        }
        let m = egui_mods(mods);
        let action = config::find_action(&self.config.bindings, key, mods, false);
        if !self.egui_owns_keyboard() || matches!(action.as_deref(), Some("show_config" | "show_history")) {
            return false;
        }
        match key {
            // egui inserts characters from Text events, exactly as a real
            // IME-driven keypress arrives through egui-winit.
            Key::Character(c) => {
                self.wins[self.cur].fifo_egui_events
                    .push(egui::Event::Text(c.as_ref().to_owned()));
            },
            Key::Named(winit::keyboard::NamedKey::Space) => {
                // A real keyboard delivers a Space as text, not as a bare
                // key code; egui's TextEdit expects the same.
                self.wins[self.cur].fifo_egui_events.push(egui::Event::Text(" ".into()));
            },
            other => {
                if let Some(k) = egui_key_of(other) {
                    let ev = |pressed| egui::Event::Key {
                        key: k,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: m,
                    };
                    self.wins[self.cur].fifo_egui_events.push(ev(true));
                    self.wins[self.cur].fifo_egui_events.push(ev(false));
                }
            },
        }
        true
    }

    fn mouse_mode(&self) -> Option<TermMode> {
        if self.modifiers.shift_key() {
            return None;
        }
        let term = self.wins[self.cur].tabs.get(self.wins[self.cur].active)?.term.lock();
        let mode = *term.mode();
        let any = mode.contains(TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(TermMode::MOUSE_DRAG)
            || mode.contains(TermMode::MOUSE_MOTION);
        any.then_some(mode)
    }

    /// Viewport cell under the pointer in protocol coordinates (1-based), or
    /// None over the tab strip or outside the grid.
    fn mouse_cell(&self, px: f32, py: f32) -> Option<(u16, u16)> {
        let strip_h = if self.show_tab_strip() { self.tab_h } else { 0.0 };
        if py < strip_h {
            return None;
        }
        let col = (px / self.cell_w).floor() as u16;
        let row = ((py - strip_h) / self.cell_h).floor() as i32;
        if col >= self.wins[self.cur].cols as u16 || row < 0 || row as usize >= self.wins[self.cur].rows {
            return None;
        }
        Some((col + 1, row as u16 + 1))
    }

    /// Modifier bits for a mouse report (4 shift, 8 alt, 16 ctrl). Shift can
    /// never be set here: it bypasses reporting entirely.
    fn mouse_mods(&self) -> u16 {
        let mut m = 0u16;
        if self.modifiers.alt_key() {
            m |= 8;
        }
        if self.modifiers.control_key() {
            m |= 16;
        }
        m
    }

    fn mouse_button_code(&self, button: MouseButton) -> u16 {
        let base = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => 0,
        };
        base | self.mouse_mods()
    }

    /// Cell under the pointer, in grid coordinates, plus which half of it.
    /// None outside the terminal area: the tab strip is not text.
    /// Visual cell under a pointer position (no scroll-offset adjustment:
    /// url spans are keyed by visual rows, which is also what painting
    /// wants).
    fn visual_cell(&self, px: f32, py: f32) -> Option<(usize, usize)> {
        let strip_h = if self.show_tab_strip() { self.tab_h } else { 0.0 };
        if py < strip_h {
            return None;
        }
        let col = (px / self.cell_w).floor() as usize;
        let row = ((py - strip_h) / self.cell_h).floor();
        if row < 0.0 || col >= self.wins[self.cur].cols || row as usize >= self.wins[self.cur].rows {
            return None;
        }
        Some((row as usize, col))
    }

    /// The URL under a pointer position, rescanning that row through the
    /// hash cache. Clicks use this rather than `url_spans` because the
    /// pointer can move without a frame being drawn between; the click must
    /// answer about the word it physically landed on.
    fn url_at(&mut self, px: f32, py: f32) -> Option<String> {
        let (row, col) = self.visual_cell(px, py)?;
        let spans = self.scan_row_cached(row);
        spans
            .into_iter()
            .find(|s| s.row == row && col >= s.start && col < s.end)
            .map(|s| s.url)
    }

    /// Find the URLs on ONE visual row of the grid.
    ///
    /// Laziness is the whole design: an underline or a click can only ever
    /// concern the row the pointer is on, so only that row is ever scanned -
    /// one row of <= a few hundred cells, with an early exit from the cell
    /// iterator, no allocation per character, and a content hash so an
    /// unchanged row reuses its spans without re-parsing at all. The former
    /// version scanned every visible row into fresh Strings on every frame;
    /// that showed up as O(row_len^2) per row (a `String` of every suffix
    /// was built while hunting the scheme) multiplied by the whole screen
    /// multiplied by every mouse-move-triggered redraw - needlessly rude to
    /// the terminal lock the PTY reader also wants.
    fn scan_row_cached(&mut self, row: usize) -> Vec<UrlSpan> {
        let (hash, text) = self.row_text(row);
        if let Some((r, h, spans)) = &self.url_cache {
            if *r == row && *h == hash {
                return spans.clone(); // tiny: a row holds at most a handful of URLs
            }
        }
        let spans = url_spans_in_row(&text, row);
        self.url_cache = Some((row, hash, spans.clone()));
        spans
    }

    /// The row's visible text plus an FNV-1a hash of it, taken under one
    /// terminal lock. The hash is the cache key; the text is usually
    /// thrown away unexamined because the row has not moved.
    fn row_text(&self, row: usize) -> (u64, Vec<char>) {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) else {
            return (hash, Vec::new());
        };
        let term = tab.term.lock();
        let content = term.renderable_content();
        let mut chars: Vec<char> = Vec::new();
        for cell in content.display_iter {
            let crow = cell.point.line.0 + content.display_offset as i32;
            if crow < 0 {
                continue;
            }
            if (crow as usize) < row {
                continue;
            }
            if (crow as usize) > row {
                break; // display_iter walks row by row
            }
            let col = cell.point.column.0;
            while chars.len() < col {
                chars.push(' ');
                hash = (hash ^ b' ' as u64).wrapping_mul(0x100_0000_01b3);
            }
            let raw = cell.c;
            let ch = if raw.is_whitespace() || (raw as u32) < 0x20 { ' ' } else { raw };
            if col == chars.len() {
                chars.push(ch);
            }
            hash = (hash ^ ch as u64).wrapping_mul(0x100_0000_01b3);
        }
        (hash, chars)
    }

    #[allow(dead_code)]
    fn scan_row(&self, row: usize) -> Vec<UrlSpan> {
        let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) else {
            return Vec::new();
        };
        let term = tab.term.lock();
        let content = term.renderable_content();
        let mut chars: Vec<char> = Vec::new();
        for cell in content.display_iter {
            let crow = cell.point.line.0 + content.display_offset as i32;
            if crow < 0 {
                continue;
            }
            if (crow as usize) < row {
                continue;
            }
            if (crow as usize) > row {
                break; // display_iter walks row by row
            }
            let col = cell.point.column.0;
            if col > chars.len() {
                chars.extend(std::iter::repeat(' ').take(col - chars.len()));
            }
            let ch = cell.c;
            if col == chars.len() {
                chars.push(if ch.is_whitespace() || (ch as u32) < 0x20 { ' ' } else { ch });
            }
        }
        url_spans_in_row(&chars, row)
    }

    fn open_url(url: &str) {
        // The portal-first, xdg-standard route; detached so a browser that
        // never forks is not our child to reap.
        match std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => log::info!("opened url: {url}"),
            Err(e) => log::warn!("xdg-open {url}: {e}"),
        }
    }

    fn cell_at(&self, px: f32, py: f32) -> Option<(Point, Side)> {
        let tab = self.wins[self.cur].tabs.get(self.wins[self.cur].active)?;
        let strip_h = if self.show_tab_strip() { self.tab_h } else { 0.0 };
        if py < strip_h {
            return None;
        }
        let col = (px / self.cell_w).floor() as usize;
        let row = ((py - strip_h) / self.cell_h).floor() as i32;
        if col >= self.wins[self.cur].cols || row < 0 || row as usize >= self.wins[self.cur].rows {
            return None;
        }
        let within = px / self.cell_w - (px / self.cell_w).floor();
        let side = if within < 0.5 { Side::Left } else { Side::Right };
        // Selection lines are grid coordinates: line 0 is the top of the
        // unscrolled screen and scrollback above it is negative, so the scroll
        // offset comes off the visual row.
        let off = tab.term.lock().grid().display_offset() as i32;
        Some((Point { line: Line(row - off), column: Column(col) }, side))
    }

    /// How many times this button was pressed in this spot within a double-click
    /// window, capped at three (three means "whole rows").
    fn count_click(&mut self, button: u8, at: (f32, f32)) -> u32 {
        let now = std::time::Instant::now();
        let count = match self.wins[self.cur].click {
            Some((b, c, p, t))
                if b == button
                    && now.duration_since(t) < std::time::Duration::from_millis(400)
                    && (at.0 - p.0).abs() < 6.0
                    && (at.1 - p.1).abs() < 6.0 =>
            {
                c + 1
            }
            _ => 1,
        };
        self.wins[self.cur].click = Some((button, count, at, now));
        count.min(3)
    }

    /// Publish the selection into the primary buffer, the way VTE does: selecting
    /// text *is* copying it, and middle click is the paste.
    fn publish_primary(&mut self) {
        if let Some(text) = self.wins[self.cur].tabs.get(self.wins[self.cur].active).and_then(|t| t.selected_text()) {
            self.copy_text(clipboard::Kind::Primary, text);
        }
    }

    fn show_error(&mut self, error: String) {
        log::warn!("{error}");
        self.wins[self.cur].notice = Some((error, Instant::now()));
        if let Some(window) = &self.wins[self.cur].window { window.request_redraw(); }
    }

    fn copy_text(&mut self, kind: clipboard::Kind, text: String) {
        let result = self.clipboard.as_ref().ok_or_else(|| "Clipboard unavailable".to_string()).and_then(|c| c.set(kind, text));
        if let Err(e) = result { self.show_error(e); }
    }

    fn request_paste(&mut self, kind: clipboard::Kind) {
        let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) else { return };
        let result = self.clipboard.as_ref().ok_or_else(|| "Clipboard unavailable".to_string()).and_then(|c| c.paste(kind, tab.id));
        if let Err(e) = result { self.show_error(e); }
    }

    /// Turn BELs into escalation where they are unseen: attention knocks at
    /// the compositor and the tab strip pulses that cell in the palette's
    /// red. A bell in the window you are looking at, on the tab you are
    /// reading, stays a bell — the visual flash already says what happened;
    /// escalation is for news nobody is watching. Shell integration hooks
    /// (scripts/shell_integration.*) ring this bell on failed commands,
    /// which is "notify me when the build breaks" without a D-Bus daemon.
    fn drain_bell_pings(&mut self) {
        for w in self.wins.iter_mut() {
            if w.bell_pulse.is_some_and(|(_, at)| at.elapsed().as_secs_f32() > 0.9) {
                w.bell_pulse = None;
                if let Some(win) = w.window.as_ref() {
                    win.request_redraw();
                }
            }
        }
        let pings: Vec<(usize, Instant)> =
            std::mem::take(&mut *self.bell_pings.lock().unwrap());
        for (tab_id, at) in pings {
            for (wi, w) in self.wins.iter_mut().enumerate() {
                if let Some(ti) = w.tabs.iter().position(|t| t.id == tab_id) {
                    let seen = wi == self.cur && w.active == ti && w.focused;
                    if !seen {
                        w.bell_pulse = Some((ti, at));
                        if let Some(win) = w.window.as_ref() {
                            win.request_user_attention(Some(
                                winit::window::UserAttentionType::Critical,
                            ));
                            win.request_redraw();
                        }
                    }
                    break;
                }
            }
        }
    }

    /// Fold dispatched OSC 133 marks into per-tab command blocks. The hard
    /// part - WHERE a mark sits - was done by the state machine at dispatch
    /// time; this only associates marks into blocks and forgets tabs that
    /// are gone (a mark racing its tab's death is simply irrelevant).
    fn drain_sems(&mut self) {
        let evs: Vec<SemanticMark> =
            std::mem::take(&mut *self.sems_ring.lock().unwrap());
        if !self.config.osc133 {
            return;
        }
        let cap = self.config.history_block_bytes;
        for SemanticMark { tab_id, code, exit, line, row, col, alt, command, when } in evs {
            // The tab the mark belongs to, wherever it lives now.
            let tab = match self
                .wins
                .iter()
                .find_map(|w| w.tabs.iter().find(|t| t.id == tab_id))
            {
                Some(t) => t,
                None => continue,
            };
            let blocks = self.sem_blocks.entry(tab_id).or_default();
            // D closes the newest open block; note which one BEFORE the
            // fold, so the capture below can attribute the text even
            // though the fold is what flips `done`.
            let closing = if code == 3 {
                blocks.iter().rposition(|b| !b.done)
            } else {
                None
            };
            if code == 0 {
                self.sem_seq += 1;
            }
            apply_sem_mark(blocks, code, exit, line, row, alt, self.sem_seq);
            if code == 2 {
                if let Some(b) = blocks.iter_mut().rev().find(|b| !b.done) {
                    b.started_at.get_or_insert(when);
                    if let Some(command) = command {
                        b.cmd_line = command;
                    }
                }
            }
            if code != 3 {
                continue;
            }
            let Some(i) = closing else { continue };
            let Some(b) = blocks.get_mut(i) else { continue };
            if !b.done {
                continue; // a stray second D: the fold ignored it
            }
            b.finished_at = Some(when);
            if alt || b.alt_seen {
                // A full-screen program owned the rows this command
                // "wrote". There is no output to capture; say so.
                b.alt_seen = true;
                continue;
            }
            // Translate the block's row IDs into TODAY'S buffer rows:
            // buffer row 0 is whichever row the ring's oldest survivor
            // is, `scrolled_out - history_size` rows past the start of
            // time. This translation is exact even after eviction,
            // which is the whole reason marks carry `row` at all.
            let (hist, prod) = tab.grid_metrics();
            let base = prod.saturating_sub(hist);
            let from = b.start_global.saturating_sub(base);
            // D's cursor line is the fresh row the next prompt will
            // paint: the output is above it. +1 includes a final
            // partial line; max() keeps at least the prompt row itself.
            // D's cursor column says whether the command's last line
            // ended with a newline: col 0 means the cursor sits on a
            // fresh row the output never touched, so that row (which
            // the next prompt will paint, and HAS painted by the time
            // we capture) must stay out. col > 0 means output shares
            // the row - include it, prompt smear and all; there is no
            // way to split a row the terminal already merged.
            let to = if col > 0 { row + 1 } else { row }.saturating_sub(base).max(from + 1);
            if let Some((txt, readable, _)) = tab.capture_rows(from, to) {
                b.truncated_head |= b.start_global < base || readable > from;
                push_block_text(b, &txt, cap);
            }
            finish_block(b);
        }
        let budget = self.config.history_bytes;
        evict_to_budget(&mut self.sem_blocks, budget);
    }

    /// The last `n` commands' captured output of `window`'s active tab
    /// (1-based; 0 or out of range = the window the user is looking at).
    /// Shared by the session bus's `mypixels last` and the MCP endpoint:
    /// one extraction core, thin transports.
    fn last_output_text(&mut self, window: u32, n: u64) -> String {
        self.drain_sems();
        let win = if window >= 1 && (window as usize) <= self.wins.len() {
            window as usize - 1
        } else {
            self.cur
        };
        self.wins
            .get(win)
            .and_then(|w| w.tabs.get(w.active))
            .and_then(|t| self.sem_blocks.get_mut(&t.id))
            .map(|q| format_last_output(q, n.max(1) as usize))
            .unwrap_or_default()
    }

    /// The history panel's rows for one tab: newest first, every field
    /// precomputed. A closed block's command line came from its C mark
    /// at dispatch; an idle prompt is kept out of the command list.
    fn history_rows_for(&self, tab_id: usize) -> Vec<egui_ui::HistoryRow> {
        self.sem_blocks.get(&tab_id).map(|q| {
            q.iter().rev().filter(|b| b.is_history_command()).map(|b| {
                let stamp = b.finished_at.or(b.started_at);
                let time = stamp
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| local_hms(d.as_secs() as i64))
                    .unwrap_or_default();
                let duration = match (b.started_at, b.finished_at) {
                    (Some(s), Some(f)) => f
                        .duration_since(s)
                        .ok()
                        .map(format_duration)
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let cmd = if b.cmd_line.is_empty() {
                    b.text.lines().next().unwrap_or_default().to_string()
                } else {
                    b.cmd_line.clone()
                };
                egui_ui::HistoryRow {
                    seq: b.seq,
                    cmd,
                    exit: b.exit,
                    done: b.done,
                    alt_seen: b.alt_seen,
                    lines: b.line_count,
                    time,
                    duration,
                }
            }).collect()
        }).unwrap_or_default()
    }

    /// What the history panel's action buttons asked for. `seq` identifies
    /// the block; the panel's own dismissal is not an action. The block's
    /// data is copied out first: the actions need `&mut self` (clipboard,
    /// scroll), which cannot coexist with a live borrow of the pool.
    fn do_history_action(&mut self, seq: u64, action: &str) {
        let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) else {
            return;
        };
        let tab_id = tab.id;
        let (prompt_line, cmd_line, text) = {
            let Some(b) = self
                .sem_blocks
                .get_mut(&tab_id)
                .and_then(|q| q.iter_mut().find(|b| b.seq == seq))
            else {
                // Evicted while the panel was open: say so, do not guess.
                self.show_error("That command left the output pool".into());
                return;
            };
            (b.prompt_line, b.cmd_line.clone(), b.readable_text().to_string())
        };
        match action {
            "copy" => {
                if text.is_empty() {
                    self.show_error("No captured output to copy".into());
                } else {
                    self.copy_text(clipboard::Kind::Clipboard, text);
                    self.wins[self.cur].notice =
                        Some(("Command output copied".to_string(), Instant::now()));
                }
            }
            "rerun" => {
                if cmd_line.is_empty() {
                    self.show_error("No command line to re-run".into());
                } else {
                    let mut bytes = cmd_line.as_bytes().to_vec();
                    bytes.push(b'\r');
                    tab.write(&bytes);
                }
            }
            // The same viewport math as `jump_command`: the offset that
            // puts the prompt line at the top is `history_size - line`.
            "view" => {
                let (hist, cur) = {
                    let term = tab.term.lock();
                    (term.grid().history_size(), term.grid().display_offset())
                };
                let want = hist.saturating_sub(prompt_line);
                let delta = want as i32 - cur as i32;
                if delta != 0 {
                    tab.scroll(alacritty_terminal::grid::Scroll::Delta(delta));
                    if let Some(w) = &self.wins[self.cur].window {
                        w.request_redraw();
                    }
                }
            }
            // The panel draws it disabled; a stray one is a no-op.
            _ => {}
        }
    }

    /// Scroll so the viewport top lands on the previous/next command's
    /// prompt. Positions are absolute logical lines; the viewport offset
    /// that shows one is `history_size - line`, clamped by the grid.
    fn jump_command(&mut self, back: bool) {
        self.drain_sems();
        let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) else {
            return;
        };
        let blocks: Vec<usize> = self
            .sem_blocks
            .get(&tab.id)
            .map(|b| b.iter().map(|b| b.prompt_line).collect())
            .unwrap_or_default();
        if blocks.is_empty() {
            return;
        }
        let (hist, cur) = {
            let term = tab.term.lock();
            (term.grid().history_size(), term.grid().display_offset())
        };
        let cur_top = hist - cur;
        let target = if back {
            blocks.iter().filter(|l| **l < cur_top).max()
        } else {
            blocks.iter().filter(|l| **l > cur_top).min()
        };
        log::debug!("jump back={back} blocks={} hist={hist} cur={cur} target={target:?}", blocks.len());
        if let Some(t) = target {
            let want = hist.saturating_sub(*t);
            log::debug!("jump -> offset {want} (delta {})", want as i32 - cur as i32);
            tab.scroll(alacritty_terminal::grid::Scroll::Delta(
                want as i32 - cur as i32,
            ));
            if let Some(w) = &self.wins[self.cur].window {
                w.request_redraw();
            }
        }
    }

    /// Hand complete APC strings to the per-tab Kitty state machine.
    /// Decoding happens here, once, on the main thread: bounded, unit
    /// tested, and never under the terminal lock.
    fn drain_apcs(&mut self) {
        let evs: Vec<(usize, Vec<u8>, usize, usize)> =
            std::mem::take(&mut *self.apcs_ring.lock().unwrap());
        for (tab_id, payload, line, col) in evs {
            if let Some(path) = &self.apc_dump.clone() {
                use std::io::Write as _;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(f, "APCFRAME {tab_id} {line} {col} {}", payload.len());
                    let _ = f.write_all(&payload);
                }
            }
            let live = self.wins.iter().any(|w| w.tabs.iter().any(|t| t.id == tab_id));
            if !live {
                continue;
            }
            log::debug!("apc drained for tab {tab_id}: {} bytes at {line}:{col}", payload.len());
            let reply = self.kitty.entry(tab_id).or_default().on_apc(&payload, line, col);
            if !reply.is_empty() {
                if let Some(tab) = self
                    .wins
                    .iter()
                    .flat_map(|w| w.tabs.iter())
                    .find(|t| t.id == tab_id)
                {
                    tab.write(&reply);
                }
            }
        }
    }

    /// Encode a key RELEASE for the active tab. Only the enhanced protocol
    /// can carry a release; the legacy path simply has nothing to say, and
    /// "send nothing" is decided explicitly here rather than by an encoder
    /// accident. A release never touches selection, scroll or activity.
    fn send_key_release(&mut self, ke: &winit::event::KeyEvent) {
        let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) else {
            return;
        };
        let (flags, app_cursor) = {
            let term = tab.term.lock();
            let m = *term.mode();
            (
                kitty_kb::active_flags(&m),
                m.contains(alacritty_terminal::term::TermMode::APP_CURSOR),
            )
        };
        if flags == 0 {
            return;
        }
        if let Some(bytes) = kitty_kb::encode(&kitty_kb::Input {
            key: &ke.logical_key,
            text: ke.text.as_deref(),
            physical: ke.physical_key,
            location: ke.location,
            release: true,
            repeat: false,
            mods: self.modifiers,
            flags,
            app_cursor,
        }) {
            if !bytes.is_empty() {
                tab.write(&bytes);
            }
        }
    }

    /// Deliver the replies the terminal queued for its programs (keyboard
    /// mode queries, cursor position reports). NOT entangled: a response
    /// belongs to the tab that was asked, not to every mirrored shell.
    fn drain_ptwrites(&mut self) {
        let evs: Vec<(usize, String)> =
            std::mem::take(&mut *self.pty_writes_ring.lock().unwrap());
        for (tab_id, text) in evs {
            if let Some(tab) = self
                .wins
                .iter()
                .flat_map(|w| w.tabs.iter())
                .find(|t| t.id == tab_id)
            {
                tab.write(text.as_bytes());
            }
        }
    }

    /// A tab executed RIS: drop its whole graphics store and the GPU
    /// textures that came from it, everywhere they were painted.
    fn drain_resets(&mut self) {
        let ids: Vec<usize> = std::mem::take(&mut *self.resets_ring.lock().unwrap());
        if ids.is_empty() {
            return;
        }
        for id in ids {
            if let Some(k) = self.kitty.get_mut(&id) {
                k.clear_all();
            }
            self.forget_gpu_images(id);
        }
        for w in self.wins.iter_mut() {
            if let Some(win) = w.window.as_ref() {
                win.request_redraw();
            }
        }
    }

    /// CSI 16 t answered from live font metrics: `CSI 6 ; h ; w t`. This is
    /// how broot's kitty renderer (and sixel detectors) learn to scale
    /// pixels to cells - without it they refuse to draw anything.
    fn drain_cell_sizes(&mut self) {
        let ids: Vec<usize> = std::mem::take(&mut *self.cell_sizes_ring.lock().unwrap());
        if ids.is_empty() {
            return;
        }
        let reply = format!(
            "\x1b[6;{};{}t",
            self.cell_h.round() as u32,
            self.cell_w.round() as u32
        );
        for w in self.wins.iter_mut() {
            for t in w.tabs.iter_mut() {
                if ids.contains(&t.id) {
                    t.write(reply.as_bytes());
                }
            }
        }
    }

    fn drain_clipboard(&mut self) {
        let events: Vec<_> = self.clipboard.as_ref().map(|c| c.events().collect()).unwrap_or_default();
        for event in events {
            match event {
                clipboard::Event::Paste { target, result: Ok(text) } => {
                    if let Some(tab) = self.wins[self.cur].tabs.iter().find(|t| t.id == target) { tab.paste(&text); }
                }
                clipboard::Event::Paste { result: Err(e), .. } | clipboard::Event::Error(e) => self.show_error(e),
            }
        }
    }

    fn handle_key(&mut self, key: &Key, kev: Option<&winit::event::KeyEvent>, event_loop: &ActiveEventLoop) {
        if self.wins[self.cur].drag.take().is_some() { self.publish_primary(); }
        // A key release is not an action, never scrolls, never selects. Its
        // only legitimate use is the Kitty keyboard protocol, where apps
        // asked for release events; everything else about a release is the
        // same key's press having already happened.
        if let Some(ke) = kev {
            if ke.state == winit::event::ElementState::Released {
                self.send_key_release(ke);
                return;
            }
        }
        // 0. The egui rename box owns the keyboard: egui consumes the keys
        //    it needs, and anything left over is swallowed here so a half-
        //    typed name can never leak into the shell. Escape is the app's
        //    own cancel, in case egui already had the focus dropped.
        if self.wins[self.cur].renaming {
            if matches!(key, Key::Named(winit::keyboard::NamedKey::Escape)) {
                self.close_rename();
            }
            return;
        }
        // 0.5. A transient egui picker (context menu, window picker) owns
        //      the keyboard the same way the rename box does: Escape closes
        //      it (through egui's own dismissal), and no key may also reach
        //      the shell underneath - the shell must not act on the very
        //      keystroke that dismisses a menu.
        if self.wins[self.cur].egui.as_ref().is_some_and(|e| e.ui_state.menu_open || e.ui_state.move_open) {
            return;
        }
        // 1. The switcher claims only its own keys (arrows, Home/End, digits
        //    and Esc with plain Alt); anything else falls through to the
        //    normal handler, so configured bindings and the terminal keep
        //    working while the switcher is open.
        if self.wins[self.cur].overview.as_ref().map(|o| o.mode) == Some(OverviewMode::Switcher) {
            if self.handle_switcher_key(key) {
                return;
            }
        }
        // 2. Configured bindings
        let overview_active = self.wins[self.cur].overview.is_some();
        if let Some(action) = config::find_action(&self.config.bindings, key, self.modifiers, overview_active) {
            self.do_action(&action, event_loop);
            // A delegated action may have moved or closed the active tab;
            // keep the highlighted tile in sync while the switcher is open.
            let cur_active = self.wins[self.cur].active;
            if let Some(ov) = self.wins[self.cur].overview.as_mut() {
                ov.selected = cur_active;
            }
            return;
        }
        // 3. Everything else goes to the active terminal
        let (app_cursor, kb_flags) = match self.wins[self.cur].tabs.get(self.wins[self.cur].active) {
            Some(t) => {
                let term = t.term.lock();
                let m = *term.mode();
                (
                    m.contains(alacritty_terminal::term::TermMode::APP_CURSOR),
                    kitty_kb::active_flags(&m),
                )
            },
            None => (false, 0),
        };
        let bytes = if kb_flags != 0 {
            match kev {
                // Enhancement flags are active: the enhanced encoder owns
                // this key. `None` from it means "nothing new to say" and
                // the legacy table (byte-identical for these keys) speaks.
                Some(ke) => kitty_kb::encode(&kitty_kb::Input {
                    key,
                    text: ke.text.as_deref(),
                    physical: ke.physical_key,
                    location: ke.location,
                    release: false,
                    repeat: ke.repeat,
                    mods: self.modifiers,
                    flags: kb_flags,
                    app_cursor,
                })
                .or_else(|| key_bytes(key, self.modifiers, app_cursor)),
                None => key_bytes(key, self.modifiers, app_cursor),
            }
        } else {
            key_bytes(key, self.modifiers, app_cursor)
        };
        if let Some(bytes) = bytes.filter(|b| !b.is_empty()) {
            // Typing counts as activity: keep the cursor solid (no blink)
            // right after a keypress.
            *self.last_activity.lock().unwrap() = std::time::Instant::now();
            // A selection is something you are about to act on; typing means you
            // moved on, and leaving the wash over new output would be a lie.
            let cur_active = self.wins[self.cur].active;
            if let Some(tab) = self.wins[self.cur].tabs.get_mut(cur_active) {
                tab.clear_selection();
                tab.scroll(alacritty_terminal::grid::Scroll::Bottom);
            }
            if let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) {
                // Entanglement: type into all entangled tabs
                if self.wins[self.cur].tabs[self.wins[self.cur].active].entangled {
                    for t in &self.wins[self.cur].tabs {
                        if t.entangled {
                            t.write(&bytes);
                        }
                    }
                } else {
                    tab.write(&bytes);
                }
            }
        }
    }

    /// Whether an egui surface is open that the user could plausibly be
    /// typing into right now. Keys belong to egui only under that
    /// condition; anything else belongs to the terminal or the bindings.
    fn egui_owns_keyboard(&self) -> bool {
        // The switcher owns navigation even when a sidebar was open before it.
        if self.wins[self.cur].overview.is_some() { return false; }
        let Some(e) = self.wins[self.cur].egui.as_ref() else {
            return false;
        };
        e.ui_state.owns_keyboard(&e.ctx)
    }

    /// Close the rename box without committing (app-side Escape).
    fn close_rename(&mut self) {
        self.wins[self.cur].renaming = false;
        if let Some(egui) = self.wins[self.cur].egui.as_mut() {
            egui.ui_state.rename_open = false;
            egui_ui::release_focus(&egui.ctx);
        }
        if let Some(w) = &self.wins[self.cur].window {
            w.request_redraw();
        }
    }

    /// Dismiss the Alt-held switcher (only if that variant is open).
    fn close_switcher(&mut self) {
        if self.wins[self.cur].overview.as_ref().map(|o| o.mode) == Some(OverviewMode::Switcher) {
            self.wins[self.cur].overview = None;
            self.switcher_open.store(false, Ordering::SeqCst);
            self.wins[self.cur].sw_thumbs.clear();
        }
        if let Some(w) = &self.wins[self.cur].window {
            w.request_redraw();
        }
    }

    /// Keys while the Alt-held switcher is open. Returns whether the key is
    /// one of the switcher's own bindings and was consumed. The switcher's
    /// bindings use Alt with arrows, Home/End, digits, Tab, Enter, or Esc.
    /// Shift-Tab cycles backwards. Other chords fall through to the normal
    /// handler (configured bindings, then the terminal). Navigation changes
    /// the active tab live, so the preview and the eventual commit are the
    /// same tab; Alt release just dismisses the popup.
    fn handle_switcher_key(&mut self, key: &Key) -> bool {
        use winit::keyboard::NamedKey;
        let alt_navigation = self.modifiers.alt_key()
            && !self.modifiers.control_key()
            && !self.modifiers.super_key()
            && (!self.modifiers.shift_key() || matches!(key, Key::Named(NamedKey::Tab)));
        if !alt_navigation {
            return false;
        }
        let n = self.wins[self.cur].tabs.len();
        if n == 0 {
            self.close_switcher();
            return true;
        }
        let screen_width = self.wins[self.cur].window.as_ref()
            .map(|w| w.inner_size().width as f32 / self.scale.max(1.0)).unwrap_or(1280.0);
        let cols = egui_ui::switcher_columns(screen_width, n);
        let handled = match key {
            Key::Named(NamedKey::Tab) => {
                let step = if self.modifiers.shift_key() { n - 1 } else { 1 };
                self.wins[self.cur].active = (self.wins[self.cur].active + step) % n;
                true
            },
            Key::Named(NamedKey::Enter) => {
                self.close_switcher();
                true
            },
            Key::Named(NamedKey::ArrowLeft) => {
                self.wins[self.cur].active = (self.wins[self.cur].active + n - 1) % n;
                true
            },
            Key::Named(NamedKey::ArrowRight) => {
                self.wins[self.cur].active = (self.wins[self.cur].active + 1) % n;
                true
            },
            Key::Named(NamedKey::ArrowUp) => {
                let r = self.wins[self.cur].active / cols;
                if r > 0 {
                    let dc = self.wins[self.cur].active % cols;
                    self.wins[self.cur].active = ((r - 1) * cols + dc).min(n - 1);
                }
                true
            },
            Key::Named(NamedKey::ArrowDown) => {
                let r = self.wins[self.cur].active / cols;
                let t = (r + 1) * cols + (self.wins[self.cur].active % cols);
                if t < n && t / cols > r {
                    self.wins[self.cur].active = t;
                }
                true
            },
            Key::Named(NamedKey::Home) => {
                self.wins[self.cur].active = 0;
                true
            },
            Key::Named(NamedKey::End) => {
                self.wins[self.cur].active = n - 1;
                true
            },
            Key::Named(NamedKey::Escape) => {
                if let Some(overlay) = self.wins[self.cur].overview {
                    self.wins[self.cur].active = overlay.original.min(n - 1);
                }
                self.close_switcher();
                true
            },
            Key::Character(c) => {
                // Alt+digit: jump to that tab (Alt is held in the switcher).
                match c.chars().next().and_then(|ch| ch.to_digit(10)) {
                    Some(d) => {
                        let idx = if d == 0 { 9 } else { (d - 1) as usize };
                        if idx < n {
                            self.wins[self.cur].active = idx;
                        }
                        true
                    },
                    None => false,
                }
            },
            _ => false,
        };
        if handled {
            // Keep the highlighted tile in sync with the active tab.
            let cur_active = self.wins[self.cur].active;
            if let Some(ov) = self.wins[self.cur].overview.as_mut() {
                ov.selected = cur_active;
            }
            if let Some(w) = &self.wins[self.cur].window {
                w.request_redraw();
            }
        }
        handled
    }

    fn do_action(&mut self, action: &str, event_loop: &ActiveEventLoop) {
        let n = self.wins[self.cur].tabs.len();
        match action {
            "new_tab" => self.add_tab(),
            "new_window" => self.make_window(event_loop, None, None, true),
            "send_tab_to_window" => {
                if self.wins.len() < 2 {
                    self.wins[self.cur].notice = Some((
                        "No other window -- Ctrl+Shift+n opens one".to_string(),
                        Instant::now(),
                    ));
                } else if self.wins.len() == 2 {
                    // Exactly two: "to the other one" needs no dialog.
                    let from = self.cur;
                    let to = 1 - from;
                    let idx = self.wins[from].active;
                    let insert = self.wins[to].tabs.len();
                    self.move_tab(from, idx, to, insert, event_loop);
                } else {
                    let mut items = Vec::new();
                    let mut targets = Vec::new();
                    for (i, w) in self.wins.iter().enumerate() {
                        if i == self.cur {
                            continue;
                        }
                        let title = w
                            .tabs
                            .get(w.active)
                            .map(|t| t.display_title())
                            .unwrap_or_else(|| "empty".to_string());
                        items.push(format!("Window {} -- {} ({} tabs)", i + 1, title, w.tabs.len()));
                        targets.push(i);
                    }
                    items.push("New window".to_string());
                    targets.push(usize::MAX);
                    if let Some(egui) = self.wins[self.cur].egui.as_mut() {
                        let st = &mut egui.ui_state;
                        st.move_items = items;
                        st.move_targets = targets;
                        st.move_pos = [12.0, self.tab_h + 6.0];
                        st.move_open = true;
                        st.move_out = None;
                    }
                }
            }
            "copy" => {
                if let Some(text) = self.wins[self.cur].tabs.get(self.wins[self.cur].active).and_then(|t| t.selected_text()) {
                    self.copy_text(clipboard::Kind::Clipboard, text);
                }
            },
            "paste" => {
                self.request_paste(clipboard::Kind::Clipboard);
            },
            "paste_primary" => self.request_paste(clipboard::Kind::Primary),
            "scroll_page_up" | "scroll_page_down" | "scroll_bottom" => {
                if let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) {
                    use alacritty_terminal::grid::Scroll;
                    tab.scroll(match action { "scroll_page_up" => Scroll::PageUp, "scroll_page_down" => Scroll::PageDown, _ => Scroll::Bottom });
                }
            },
            "close_tab" => self.close_tab_at(self.wins[self.cur].active, event_loop),
            "prev_command" => self.jump_command(true),
            "next_command" => self.jump_command(false),
            "reopen_tab" => {
                // The shell that died comes back where it died. The title it
                // carried is remembered for context we do not yet spend;
                // the directory is the whole value of this feature.
                if let Some((_title, cwd)) = self.closed_ring.pop_back() {
                    self.add_tab_with(cwd, None);
                }
            }
            "prev_tab" | "prev_tab_row" => {
                if n > 1 {
                    self.wins[self.cur].active = (self.wins[self.cur].active + n - 1) % n;
                }
            },
            "next_tab" | "next_tab_row" => {
                if n > 1 {
                    self.wins[self.cur].active = (self.wins[self.cur].active + 1) % n;
                }
            },
            "first_tab" => self.wins[self.cur].active = 0,
            "last_tab" => self.wins[self.cur].active = n.saturating_sub(1),
            "move_prev_tab" => {
                if n > 1 && self.wins[self.cur].active > 0 {
                    let a = self.wins[self.cur].active;
                    self.wins[self.cur].tabs.swap(a, a - 1);
                    self.wins[self.cur].active -= 1;
                }
            },
            "move_next_tab" => {
                if n > 1 && self.wins[self.cur].active < n - 1 {
                    let a = self.wins[self.cur].active;
                    self.wins[self.cur].tabs.swap(a, a + 1);
                    self.wins[self.cur].active += 1;
                }
            },
            "move_first_tab" => {
                if n > 1 && self.wins[self.cur].active > 0 {
                    let a = self.wins[self.cur].active;
                    let t = self.wins[self.cur].tabs.remove(a);
                    self.wins[self.cur].tabs.insert(0, t);
                    self.wins[self.cur].active = 0;
                }
            },
            "move_last_tab" => {
                if n > 1 && self.wins[self.cur].active < n - 1 {
                    let a = self.wins[self.cur].active;
                    let t = self.wins[self.cur].tabs.remove(a);
                    self.wins[self.cur].tabs.push(t);
                    self.wins[self.cur].active = n - 1;
                }
            },
            "toggle_entangle" => {
                let cur_active = self.wins[self.cur].active;
                if let Some(t) = self.wins[self.cur].tabs.get_mut(cur_active) {
                    t.entangled = !t.entangled;
                }
            },
            "next_color" => {
                let cur_active = self.wins[self.cur].active;
                if let Some(t) = self.wins[self.cur].tabs.get_mut(cur_active) {
                    t.group = if t.group >= 8 { 0 } else { t.group + 1 };
                }
            },
            "prev_color" => {
                let cur_active = self.wins[self.cur].active;
                if let Some(t) = self.wins[self.cur].tabs.get_mut(cur_active) {
                    t.group = if t.group <= 0 || t.group >= 8 { 7 } else { t.group - 1 };
                }
            },
            "toggle_hide_tabs" => {
                self.wins[self.cur].tablist_hidden = !self.wins[self.cur].tablist_hidden;
                self.sync_term_size();
            },
            "config_reload" => {
                self.reload_config();
            },
            "show_config" => {
                self.close_switcher();
                *self.switcher_alt_at.lock().unwrap() = None;
                self.switcher_show_req.store(false, Ordering::SeqCst);
                // Toggle: the chord that summoned the panel is the natural
                // way to dismiss it (same as every transient overlay here).
                if let Some(e) = self.wins[self.cur].egui.as_mut() {
                    e.ui_state.toggle_panel(egui_ui::RightPanel::Settings);
                    egui_ui::release_focus(&e.ctx);
                }
                if let Some(w) = &self.wins[self.cur].window {
                    w.request_redraw();
                }
            },
            "goto_tab_0" | "goto_tab_1" | "goto_tab_2" | "goto_tab_3" | "goto_tab_4"
            | "goto_tab_5" | "goto_tab_6" | "goto_tab_7" | "goto_tab_8" | "goto_tab_9" => {
                let i = action.chars().last().unwrap().to_digit(10).unwrap() as usize;
                let target = i;
                if target < n {
                    self.wins[self.cur].active = target;
                }
            },
            "enlarge_font" => self.zoom_term(true),
            "reset_font" => {
                if (self.term_pt - self.orig_term_pt).abs() >= 0.5 {
                    self.term_pt = self.orig_term_pt;
                    self.rebuild_term_font();
                }
            }
            "send_tab_new_window" => {
                self.pending_move = Some(usize::MAX);
                for w in self.window_slot.lock().unwrap().iter() {
                    w.request_redraw();
                }
            }
            "copy_as_image" => {
                self.wins[self.cur].shot = true;
                for w in self.window_slot.lock().unwrap().iter() {
                    w.request_redraw();
                }
            }
            // The in-app twin of `mypixels last --copy`: the newest
            // finished block of the pool, straight onto the clipboard.
            "copy_last_output" => {
                let text = self.last_output_text(0, 1);
                if text.is_empty() {
                    self.show_error("No captured output to copy".into());
                } else {
                    self.copy_text(clipboard::Kind::Clipboard, text);
                    self.wins[self.cur].notice =
                        Some(("Last output copied".to_string(), Instant::now()));
                }
            }
            "show_history" => {
                let opening = !self
                    .wins[self.cur]
                    .egui
                    .as_ref()
                    .is_some_and(|e| e.ui_state.panel_is(egui_ui::RightPanel::History));
                // The rows are built before the egui borrow: the builder
                // reads the pool, the toggle writes the UI state.
                let rows = if opening {
                    self.wins[self.cur]
                        .tabs
                        .get(self.wins[self.cur].active)
                        .map(|t| self.history_rows_for(t.id))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                if let Some(egui) = self.wins[self.cur].egui.as_mut() {
                    let st = &mut egui.ui_state;
                    st.toggle_panel(egui_ui::RightPanel::History);
                    egui_ui::release_focus(&egui.ctx);
                    st.history_sel = None;
                    st.history_rows = rows;
                }
            }
            // The lossless path: the store keeps the untouched upload bytes
            // per image id, so this never round-trips through pixels.
            "copy_last_image" => {
                let src = self
                    .wins[self.cur]
                    .tabs
                    .get(self.wins[self.cur].active)
                    .and_then(|t| {
                        self.kitty.get(&t.id).and_then(|k| {
                            k.placements()
                                .iter()
                                .rev()
                                .find_map(|p| k.image(p.image_id).map(|img| (*img.source).to_vec()))
                        })
                    });
                match src {
                    Some(bytes) => match self.clipboard.as_ref() {
                        None => self.show_error("Copy last image: no clipboard backend".into()),
                        Some(cb) => match cb.set_image(clipboard::Kind::Clipboard, bytes) {
                            Ok(()) => {
                                self.wins[self.cur].notice =
                                    Some(("Image copied to clipboard".to_string(), Instant::now()));
                            }
                            Err(e) => self.show_error(format!("Copy last image: {e}")),
                        },
                    },
                    None => self.show_error("No image to copy".into()),
                }
            }
            "shrink_font" => self.zoom_term(false),
            "select_all" => {
                let cur_active = self.wins[self.cur].active;
                if let Some(tab) = self.wins[self.cur].tabs.get_mut(cur_active) {
                    tab.select_all();
                }
                // VTE feeds the primary selection from select-all too.
                self.publish_primary();
            },
            "rename_tab" => {
                // The rename box is an egui text entry, so it and the Alt-
                // held switcher cannot share the screen: they share Mod1,
                // and only one of them may hold the keyboard. With the
                // switcher open, the chord therefore *closes* it and hands
                // over to the rename UI.
                self.close_switcher();
                let title =
                    self.wins[self.cur].tabs.get(self.wins[self.cur].active).map(|t| t.display_title()).unwrap_or_default();
                if let Some(egui) = self.wins[self.cur].egui.as_mut() {
                    let y = self.tab_h + 6.0;
                    let st = &mut egui.ui_state;
                    st.rename_draft = title;
                    st.rename_seed = true;
                    st.rename_pos = [12.0, y];
                    st.rename_open = true;
                    st.rename_out = None;
                    self.wins[self.cur].renaming = true;
                } else {
                    log::warn!("rename_tab: egui unavailable");
                }
            },
            // Not yet ported
            "show_search" | "show_search_all" | "search_prev" | "search_next" => {
                log::debug!("unported action: {action}");
            },
            _ => {},
        }
        if let Some(w) = &self.wins[self.cur].window {
            w.request_redraw();
        }
    }
}

/// Local HH:MM. `std::time` has no calendar/local conversion, and pulling in a
/// date-time crate for a two-number clock label is not worth it.
fn local_hhmm() -> (u32, u32) {
    // SAFETY: `time` accepts null; `localtime_r` writes the caller's own tm and
    // is the thread-safe variant (unlike `localtime`).
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (
            tm.tm_hour.max(0) as u32,
            tm.tm_min.max(0) as u32,
        )
    }
}

/// Local HH:MM:SS for a unix timestamp (the history panel's clocks).
fn local_hms(secs: i64) -> String {
    // SAFETY: as in `local_hhmm` - `localtime_r` writes the caller's tm.
    unsafe {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:02}:{:02}:{:02}",
            tm.tm_hour.max(0),
            tm.tm_min.max(0),
            tm.tm_sec.max(0)
        )
    }
}

/// A duration the panel can read at a glance: ms under a second, one
/// decimal second under a minute, minutes and seconds beyond.
fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{} s", ms as f32 / 1000.0)
    } else {
        format!("{} m {:02} s", ms / 60_000, (ms / 1000) % 60)
    }
}

fn tab_title(t: &Tab, selected: bool, max_len: usize, cur_len: usize, shorten: &str) -> String {
    let name = t.display_title();
    let limit = if selected { cur_len } else { max_len };
    if limit > 0 && name.chars().count() > limit {
        name.chars().take(limit).collect::<String>() + shorten
    } else { name }
}

/// Cell backgrounds for an offscreen terminal preview. The switcher crops
/// the reserved strip region before displaying the terminal thumbnail.
/// Signature of a whole frame, so a preview buffer is only re-shaped when its
/// tab actually changed. Same idea as `run_sig`, folded over the frame.
fn frame_sig(fd: &FrameData, cell_w: f32) -> u64 {
    fn mix(h: &mut u64, v: u64) {
        *h ^= v;
        *h = h.wrapping_mul(0x100000001b3);
    }
    let mut h: u64 = 0xcbf29ce484222325;
    mix(
        &mut h,
        fd.cursor
            .map(|c| (c.0 as u64) << 32 | c.1 as u64)
            .unwrap_or(u64::MAX),
    );
    for color in fd.fg.iter().chain(fd.bg.iter()).chain(fd.cursor_color.iter()) {
        mix(&mut h, color.to_bits() as u64);
    }
    mix(&mut h, fd.runs.len() as u64);
    for run in &fd.runs {
        mix(&mut h, run.row as u64);
        mix(&mut h, run.col as u64);
        mix(&mut h, run_sig(run, cell_w));
        for b in run.text.as_bytes() {
            mix(&mut h, *b as u64);
        }
        for c in run.fg.iter().chain(run.bg.iter()) {
            mix(&mut h, (c.clamp(0.0, 1.0) * 255.0).round() as u64);
        }
    }
    mix(&mut h, (cell_w * 1000.0).round() as u64);
    h
}

/// Rects for an offscreen frame: window background, tab strip, cell backgrounds
/// that differ from them, and the cursor. The live `gpu.rects` cannot be reused
/// -- it belongs to the active tab and carries the overlay.
#[allow(clippy::too_many_arguments)]
fn preview_rects(
    fd: &FrameData,
    bg: &[f32; 3],
    bar: &[f32; 3],
    fg: &[f32; 3],
    cell_w: f32,
    cell_h: f32,
    strip_h: f32,
    rows: usize,
    w: u32,
    h: u32,
) -> Vec<gpu::RectVertex> {
    let wf = w as f32;
    let hf = h as f32;
    let mut v: Vec<gpu::RectVertex> = Vec::with_capacity((fd.runs.len() + 3) * 6);
    let mut push = |x: f32, y: f32, rw: f32, rh: f32, c: [f32; 4]| {
        v.extend_from_slice(&gpu::rect_vertices(x, y, rw, rh, c, 0.0, wf, hf))
    };
    // Opaque: the offscreen is a picture of the window, not a see-through one.
    push(0.0, 0.0, wf, hf, [bg[0], bg[1], bg[2], 1.0]);
    if strip_h > 0.0 {
        push(0.0, 0.0, wf, strip_h, [bar[0], bar[1], bar[2], 1.0]);
    }
    for run in &fd.runs {
        if (run.row as usize) >= rows {
            continue;
        }
        let same = (run.bg[0] - bg[0]).abs() < 0.004
            && (run.bg[1] - bg[1]).abs() < 0.004
            && (run.bg[2] - bg[2]).abs() < 0.004;
        if !same {
            push(
                run.col as f32 * cell_w,
                strip_h + run.row as f32 * cell_h,
                run.width as f32 * cell_w,
                cell_h,
                [run.bg[0], run.bg[1], run.bg[2], 1.0],
            );
        }
    }
    if let Some((cx, cy)) = fd.cursor {
        push(
            cx as f32 * cell_w,
            strip_h + cy as f32 * cell_h,
            cell_w,
            cell_h,
            [fg[0], fg[1], fg[2], 0.55],
        );
    }
    v
}

/// The command line's leading flags and its command, cleanly separated.
/// Flags are only flags *before* the first positional argument: `mypixels -s
/// sh -c "sleep 1"` hands `-s` to us and `-c "sleep 1"` to the shell.
fn leading_flags() -> Vec<String> {
    std::env::args().skip(1).take_while(|a| a.starts_with('-')).collect()
}

fn positional_terms() -> Vec<String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().position(|a| !a.starts_with('-')) {
        Some(i) => args[i..].to_vec(),
        None => vec![],
    }
}

/// A PATH lookup, because a remote request is not trusted to name a real
/// program and pty spawn failures are fatal in this codebase.
fn command_exists(program: &str) -> bool {
    if program.contains('/') {
        return std::path::Path::new(program).exists();
    }
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .any(|dir| dir.join(program).is_file())
    })
}

/// The program every new tab runs when nothing else was asked for.
fn term_command() -> (String, Vec<String>) {
    let terms = positional_terms();
    if terms.is_empty() {
        (std::env::var("SHELL").unwrap_or_else(|_| "bash".into()), vec![])
    } else {
        (terms[0].clone(), terms[1..].to_vec())
    }
}

/// Parse a test key spec like "alt+shift+t", "ctrl+c", "esc", "f5".
/// Test hook: read key specs from the $MYPIXELS_KEYS FIFO in a background
/// thread and forward them over a channel. Returns None when unset.
fn spawn_key_reader(
    window_slot: Arc<Mutex<Vec<Arc<Window>>>>,
) -> Option<std::sync::mpsc::Receiver<String>> {
    let path = std::env::var("MYPIXELS_KEYS")
        .or_else(|_| std::env::var("SPIKE_KEYS"))
        .ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        // Open the FIFO read-write: a FIFO opened O_RDONLY sees EOF (and
        // blocks on the next open) every time the last writer closes, and a
        // harness that closes and reopens per line can slip its next write
        // into the gap between EOF and reopen -- a broken pipe the harness
        // blames on a dead app. Holding a writer ourselves means the read
        // side never EOFs and a reader always exists.
        loop {
            let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            else {
                return;
            };
            for line in BufReader::new(file).lines() {
                let Ok(line) = line else {
                    break; // EOF: reopen below
                };
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if tx.send(line).is_err() {
                    return;
                }
                // Wake the event loop: keys are drained on RedrawRequested, and
                // an idle terminal produces none on its own.
                if let Ok(slot) = window_slot.lock() {
                    for w in slot.iter() {
                        w.request_redraw();
                    }
                }
                // Wake the UI thread so the spec is processed promptly.
                for window in window_slot.lock().unwrap().iter() {
                    window.request_redraw();
                }
            }
        }
    });
    Some(rx)
}

impl App {
/// One more window of this session: window, GPU, egui, and a first tab
    /// running `cmd` (default shell) in `cwd`. Called at startup and on the
    /// session bus' NewWindow -- `mypixels -n` from any shell lands here.
    /// `with_tab` = false builds a bare window, for callers that immediately
    /// hand it a tab of their own (detach-to-new-window).
    fn make_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        cmd: Option<(String, Vec<String>)>,
        cwd: Option<std::path::PathBuf>,
        with_tab: bool,
    ) {
        {
            // The metrics were computed at scale 1.0 and the window is
            // created in physical pixels, so apply the display's factor
            // first (a no-op on a 1x screen).
            if let Some(m) = event_loop.primary_monitor() {
                self.set_scale(m.scale_factor() as f32);
            }
            // The template grid (App.cols/rows), not any window's: a new
            // window starts from the configured size like the first one did.
            let size = winit::dpi::PhysicalSize::new(
                (self.cols as f32 * self.cell_w) as u32,
                (self.rows as f32 * self.cell_h + self.tab_h) as u32,
            );
            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_inner_size(size)
                            .with_maximized(self.config.default_maximize)
                            // Only worth requesting when we will use it: an
                            // ARGB visual costs the compositor a second blend.
                            .with_transparent(self.config.term_transparency > 0.0)
                            .with_title("mypixels")
                            // Compositor identity: this is the wayland app_id
                            // (and the X11 WM_CLASS), which the original set
                            // to "mypixels" through Gtk.Application. Rules
                            // people wrote for the original -- open on
                            // workspace, no decorations, opacity exceptions
                            // -- must keep matching the rewrite.
                            .with_name("mypixels", "mypixels")
                            .with_window_icon(app_icon()),
                    )
                    .expect("window"),
            );
            // The window may have landed on a monitor with a different factor
            // than the primary (or the backend still reports 1.0 before the
            // first configure); the first frame must already be right.
            let sf = window.scale_factor() as f32;
            if sf != self.scale {
                self.set_scale(sf);
                let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(
                    (self.cols as f32 * self.cell_w).round() as u32,
                    (self.rows as f32 * self.cell_h + self.tab_h).round() as u32,
                ));
            }
            // Never keep a window larger than its monitor (the configured
            // size is usually the whole screen; nested/test displays are
            // smaller).
            if !self.config.default_maximize {
                if let Some(monitor) = window.current_monitor() {
                    let ms = monitor.size();
                    let ws = window.inner_size();
                    if ws.width > ms.width || ws.height > ms.height {
                        let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(
                            ws.width.min(ms.width),
                            ws.height.min(ms.height),
                        ));
                    }
                }
            }
            // The window joins the session list before its GPU: init_gpu and
            // the egui init it performs fill in the new Win's fields, and
            // everything in between already speaks wins[cur].
            let cols = self.cols;
            let rows = self.rows;
            self.wins.push(Win {
                window: Some(Arc::clone(&window)),
                gpu: None,
                egui: None,
                tabs: Vec::new(),
                active: 0,
                dims: Dims { cols, rows },
                cols,
                rows,
                tab_hits: Vec::new(),
                add_hit: (0.0, 0.0),
                tab_close_hits: Vec::new(),
                tab_close_press: None,
                pointer: (0.0, 0.0),
                mouse_button: None,
                mouse_last: None,
                drag: None,
                click: None,
                wheel_pixels: 0.0,
                hover_tab: usize::MAX,
                hover_add: false,
                tablist_hidden: self.config.tablist_hide,
                renaming: false,
                overview: None,
                sw_thumbs: Vec::new(),
                thumb_rr: 0,
                autoscroll_next: None,
                shot: false,
                tab_drag: None,
                tab_drag_moved: false,
                fifo_egui_events: Vec::new(),
                notice: None,
                bell_pulse: None,
                focused: true,
            });
            // The newest window is the one the user just asked for.
            self.cur = self.wins.len() - 1;
            self.init_gpu(event_loop, &window);
            let slot = Arc::clone(&self.window_slot);
            if self.theme_watcher.is_none() {
            match theme::Watcher::new(self.config.theme_files.clone(), move || {
                if let Ok(slot) = slot.lock() {
                    for w in slot.iter() {
                        w.request_redraw();
                    }
                }
            }) {
                Ok(watcher) => self.theme_watcher = Some(watcher),
                Err(e) => self.show_error(format!("Theme watcher unavailable: {e}")),
            }
            }
            if self.clipboard.is_none() {
            let slot = Arc::clone(&self.window_slot);
            match clipboard::Clipboard::new(clipboard::Backend::for_window(&window), move || {
                if let Ok(slot) = slot.lock() {
                    for w in slot.iter() {
                        w.request_redraw();
                    }
                }
            }) {
                Ok(clipboard) => self.clipboard = Some(clipboard),
                Err(e) => self.show_error(format!("Clipboard unavailable: {e}")),
            }
            }
            self.window_slot.lock().unwrap().push(Arc::clone(&window));
            log::info!("session: window {} up", self.wins.len());
            self.sync_term_size();
            if with_tab {
                // Every ordinary window is born with one tab: the shell the
                // request asked for, in the directory it asked from.
                self.add_tab_with(cwd, cmd);
            }
        }
        let _ = event_loop;
    }

    /// Visual-bell strength: 1.0 at the BEL, fading to 0 over 180ms.
    fn bell_flash(&self) -> f32 {
        let b = self.bell.lock().unwrap();
        match *b {
            Some(t) => {
                let e = t.elapsed().as_secs_f32();
                if e < 0.18 {
                    1.0 - e / 0.18
                } else {
                    0.0
                }
            }
            None => 0.0,
        }
    }
}

/// Lift `bg` toward a neutral grey by `k` (see bell_flash).
fn bell_mix(bg: [f32; 3], k: f32) -> [f32; 3] {
    if k <= 0.0 {
        return bg;
    }
    let w = k * 0.5;
    std::array::from_fn(|i| bg[i] + (0.35 - bg[i]) * w)
}

/// A 32x32 window icon drawn in code (a prompt bar and block cursor on a
/// dark field) -- no asset to ship, and the XDG icon lookup is not our job.
/// Wayland largely ignores window icons; X11 and window switchers use it.
fn app_icon() -> Option<winit::window::Icon> {
    static ICON: OnceLock<Option<Arc<winit::window::Icon>>> = OnceLock::new();
    ICON.get_or_init(|| {
        const S: usize = 32;
        let mut px = vec![0u8; S * S * 4];
        for y in 0..S {
            for x in 0..S {
                let inside = (2..30).contains(&x) && (2..30).contains(&y);
                let bar = (20..24).contains(&y) && (6..14).contains(&x);
                let block = (8..24).contains(&y) && (18..26).contains(&x);
                let c: [u8; 4] = if bar || block {
                    [0xa8, 0xc8, 0x44, 0xff]
                } else if inside {
                    [0x18, 0x18, 0x18, 0xff]
                } else {
                    [0, 0, 0, 0]
                };
                px[(y * S + x) * 4..][..4].copy_from_slice(&c);
            }
        }
        winit::window::Icon::from_rgba(px, S as u32, S as u32).ok().map(Arc::new)
    })
    .as_ref()
    .map(|a| (**a).clone())
}

/// Where `active` lands when tabs `a` and `b` trade places.
/// One URL found on one VISUAL row of the grid, in cell columns.
#[derive(Clone)]
struct UrlSpan {
    row: usize,
    start: usize,
    end: usize,
    url: String,
}

/// URL spans within one row of cells. Deliberately dumb: terminals print
/// URLs inside quotes, brackets and prose, so the rule is "start at the
/// scheme, stop at whitespace", then peel trailing punctuation one
/// character at a time - a ')' is prose only while it outranks its
/// openings, a '.' belongs to the URL until it ends a sentence.
///
/// Cost shape matters as much as the rules: the scheme test compares
/// characters in place (no per-suffix Strings - that made this O(n^2) in
/// the row length), and exactly one small String is allocated per URL
/// found, not per character examined.
fn url_spans_in_row(chars: &[char], row: usize) -> Vec<UrlSpan> {
    fn starts(chars: &[char], i: usize, pat: &str) -> bool {
        if i + pat.chars().count() > chars.len() {
            return false;
        }
        chars[i..].iter().zip(pat.chars()).all(|(c, p)| *c == p)
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != 'h' || !(starts(chars, i, "https://") || starts(chars, i, "http://")) {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut opens = 0usize;
        let mut closes = 0usize;
        while j < chars.len() && chars[j] != ' ' && !chars[j].is_control() {
            match chars[j] {
                '(' => opens += 1,
                ')' => closes += 1,
                _ => {}
            }
            j += 1;
        }
        // Peel prose from the end, index-based: balance-aware for ')'.
        let mut e = j;
        while e > i {
            let last = chars[e - 1];
            let prose = match last {
                ')' => closes > opens,
                '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | ']' | '}' | '>' => true,
                _ => break,
            };
            if !prose {
                break;
            }
            e -= 1;
            if last == ')' {
                closes -= 1;
            }
        }
        let host_starts = i + if starts(chars, i, "https://") { 8 } else { 7 };
        let has_dot = chars[host_starts..e].iter().any(|c| *c == '.');
        let word: String = chars[i..e].iter().collect();
        if word.len() >= 9 && has_dot {
            out.push(UrlSpan { row, start: i, end: e, url: word });
        }
        i = j.max(i + 1);
    }
    out
}

/// One shell command delimited by OSC 133 A..D marks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandBlock {
    /// Absolute logical line of the prompt, as the state machine saw it
    /// when the prompt-start mark was dispatched.
    pub prompt_line: usize,
    /// Command exit status, when the shell reported one.
    pub exit: Option<i32>,
    /// A D mark has landed. `exit == None` means "closed without a
    /// status", not "still open" - the two states must not share a
    /// sentinel, or a second D would reopen a finished command.
    pub done: bool,
    /// Monotonic open order across the whole session, so budget
    /// eviction can find the globally oldest closed block instead of
    /// guessing per tab.
    pub seq: u64,
    /// C and D wall clocks: command execution time excludes time spent
    /// sitting at the prompt or editing the input.
    pub started_at: Option<std::time::SystemTime>,
    pub finished_at: Option<std::time::SystemTime>,
    /// A prompt alone is not a running command. Set only by C.
    pub executing: bool,
    /// Buffer-row ID (survives scrollback eviction) of the prompt when
    /// the A mark landed. Capture starts AT the prompt so the echoed
    /// command rides along in the block.
    pub start_global: usize,
    /// The alternate screen owned the display at one of this command's
    /// marks: its "output" was a TUI's redraws, not capturable text.
    pub alt_seen: bool,
    /// Captured output: the prompt row (with the command) and everything
    /// the command wrote, taken in one at the D mark. Empty while the
    /// block's text lives in `compressed`.
    pub text: String,
    /// The closed block's text, compressed at the D mark. The in-progress
    /// block stays plain (it is appended to hot); only finished output is
    /// worth a codec. The budget counts this, not the inflated size.
    /// Layout: 8-byte little-endian inflated length, then the lz4 frame
    /// (the codec wants the destination length up front).
    pub compressed: Option<Vec<u8>>,
    /// The capture could not reach as far back as the command began
    /// (output larger than the scrollback), or the block cap clipped the
    /// tail. Say so; never silently drop bytes.
    pub truncated_head: bool,
    pub truncated_tail: bool,
    /// Input copied at the C mark, before the command's output can erase
    /// it. Available while running as well as after completion.
    pub cmd_line: String,
    /// Captured output's line count, for the history panel's details.
    pub line_count: usize,
}

impl CommandBlock {
    pub fn is_history_command(&self) -> bool {
        self.executing || self.done
    }

    /// Stored size for budget accounting: the compressed form when the
    /// block closed, the live text while it runs.
    pub fn stored_len(&self) -> usize {
        self.compressed.as_ref().map(|c| c.len()).unwrap_or(self.text.len())
    }

    /// Readable output. A closed block inflates on first read and drops
    /// its compressed copy, so repeated queries pay the codec once.
    pub fn readable_text(&mut self) -> &str {
        if let Some(c) = self.compressed.take() {
            let (len, frame) = if c.len() > 8 {
                let len = u64::from_le_bytes(c[0..8].try_into().unwrap());
                (len as usize, &c[8..])
            } else {
                (0, &c[..])
            };
            self.text = match lz4_flex::decompress(frame, len) {
                Ok(t) => String::from_utf8_lossy(&t).into_owned(),
                Err(e) => format!("# (pool block unreadable: {e})"),
            };
        }
        &self.text
    }
}

/// Finalize output metadata and compress it. The command was captured at C,
/// using terminal cell positions, before any output could erase it. A/D-only
/// integrations retain the legacy first-line fallback.
fn finish_block(b: &mut CommandBlock) {
    b.line_count = b.text.lines().count();
    if b.cmd_line.is_empty() {
        b.cmd_line = b.text.lines().next().unwrap_or_default().trim_end().to_string();
    }
    // The compression decision is unchanged: blocks too small to be worth
    // a frame header, and ones the codec cannot shrink, stay plain - the
    // budget counts stored bytes either way.
    if b.text.len() < 512 {
        return;
    }
    let frame = lz4_flex::compress(b.text.as_bytes());
    let mut c = Vec::with_capacity(8 + frame.len());
    c.extend_from_slice(&(b.text.len() as u64).to_le_bytes());
    c.extend_from_slice(&frame);
    if c.len() < b.text.len() {
        b.compressed = Some(c);
        b.text.clear();
    }
}

/// Append output to a block honoring the per-block byte cap by keeping
/// the TAIL (the interesting end of a build log is its last error) and
/// flagging the clip. Zero-cap callers mean "no cap".
fn push_block_text(b: &mut CommandBlock, chunk: &str, cap: usize) {
    if chunk.is_empty() {
        return;
    }
    b.text.push_str(chunk);
    if cap > 0 && b.text.len() > cap {
        let keep = cap / 2;
        let lost = b.text.len() - keep;
        // Cut on a char boundary.
        let mut split = b.text.len() - keep;
        while !b.text.is_char_boundary(split) {
            split += 1;
        }
        b.text = format!("[... {lost} bytes omitted ...]\n{}", &b.text[split..]);
        b.truncated_tail = true;
    }
}

/// Enforce the global pool budget: evict the globally oldest CLOSED
/// block until under budget. Open blocks are never evicted (their
/// command is still writing), and a tab is never emptied of its last
/// closed block unless there is nothing else to give.
fn evict_to_budget(
    blocks: &mut std::collections::HashMap<usize, std::collections::VecDeque<CommandBlock>>,
    budget: usize,
) {
    loop {
        let total: usize =
            blocks.values().flat_map(|q| q.iter()).map(|b| b.stored_len()).sum();
        if total <= budget {
            return;
        }
        // Globally oldest CLOSED block goes first (LRU by open order).
        // A tab keeps its last closed block for as long as any other
        // tab can pay; only when the budget demands the impossible do
        // we strip those too. Open blocks are sacred: their command is
        // still writing.
        let mut done = false;
        for protect_last_in_line in [true, false] {
            let mut pick: Option<(u64, usize, usize)> = None; // (seq, tab, idx)
            for (tab, q) in blocks.iter() {
                let closed = q.iter().filter(|b| b.done).count();
                if protect_last_in_line && closed < 2 {
                    continue;
                }
                for (i, b) in q.iter().enumerate() {
                    if b.done && pick.map_or(true, |(s, _, _)| b.seq < s) {
                        pick = Some((b.seq, *tab, i));
                    }
                }
            }
            if let Some((_, tab, i)) = pick {
                if let Some(q) = blocks.get_mut(&tab) {
                    q.remove(i);
                }
                done = true;
                break;
            }
        }
        if !done {
            return; // nothing closed anywhere: over budget is temporarily sacred
        }
    }
}

/// Fold one OSC 133 mark into a tab's block ring (position and text are
/// the caller's business; this only owns block identity and closure).
fn apply_sem_mark(
    blocks: &mut std::collections::VecDeque<CommandBlock>,
    code: u8,
    exit: Option<i32>,
    line: usize,
    row: usize,
    alt: bool,
    seq: u64,
) {
    match code {
        // A: prompt start opens a block.
        0 => {
            blocks.push_back(CommandBlock {
                prompt_line: line,
                seq,
                exit: None,
                done: false,
                start_global: row,
                alt_seen: alt,
                text: String::new(),
                compressed: None,
                truncated_head: false,
                truncated_tail: false,
                cmd_line: String::new(),
                line_count: 0,
                started_at: None,
                finished_at: None,
                executing: false,
            });
            while blocks.len() > 512 {
                blocks.pop_front();
            }
        },
        // C starts execution; A only opened a prompt awaiting input.
        2 => {
            if let Some(b) = blocks.iter_mut().rev().find(|b| !b.done) {
                b.executing = true;
                b.alt_seen |= alt;
            }
        },
        // D: command end closes the newest still-open block. A D with no
        // open A (marks started mid-session, or a shell that skips A) is
        // not an error - nothing to close, and the next A opens fresh.
        3 => {
            if let Some(b) = blocks.iter_mut().rev().find(|b| !b.done) {
                b.exit = exit;
                b.done = true;
            }
        },
        // B (prompt end) carries no position the v1 UI needs.
        _ => {},
    }
}

/// Render the last `n` commands' captured output for `mypixels last`.
/// A single block is bare text - it is meant to be piped or pasted.
/// Several blocks get `#` headers carrying position and exit status,
/// because then the reader is navigating, not consuming. Truncation and
/// full-screen-program notes ride along as comments; a block that
/// captured nothing still says so rather than vanishing.
fn format_last_output(q: &mut std::collections::VecDeque<CommandBlock>, n: usize) -> String {
    // The newest n blocks that actually have something to show, emitted
    // oldest-first: the newest command lands last, right where the eye
    // already is - the prompt below. v1 captures a block's text only at
    // its D mark, so the very command that invoked this query - still
    // open, therefore textless - would otherwise occupy slot 1 of
    // `last 1 --copy` and copy nothing. It is skipped, not listed.
    let mut blocks: Vec<&mut CommandBlock> =
        q.iter_mut().rev().filter(|b| b.done || !b.text.is_empty()).take(n).collect();
    blocks.reverse();
    let count = blocks.len();
    let mut out = String::new();
    for (i, b) in blocks.iter_mut().enumerate() {
        // Copy the flags before inflating: the returned `&str` holds the
        // mutable borrow for the rest of the body.
        let (done, exit, alt, th, tt) = (b.done, b.exit, b.alt_seen, b.truncated_head, b.truncated_tail);
        // Inflate once; the rest of the loop reads `text`, never the
        // compressed form.
        let text = b.readable_text();
        if count > 1 {
            let status = match (done, exit) {
                (false, _) => "running".into(),
                (true, Some(0)) => "ok".into(),
                (true, Some(code)) => format!("exit {code}"),
                (true, None) => "no status".into(),
            };
            out.push_str(&format!(
                "\n# ── {} of {} · {status} ──\n",
                i + 1,
                count
            ));
        }
        if alt {
            out.push_str("# (full-screen program; output not captured)\n");
        } else if text.is_empty() && done && !th {
            out.push_str("# (no output captured)\n");
        } else {
            out.push_str(text);
            if !text.is_empty() && !text.ends_with('\n') {
                out.push('\n');
            }
        }
        if th || tt {
            let which = match (th, tt) {
                (true, true) => "head and tail",
                (true, false) => "head",
                _ => "tail",
            };
            out.push_str(&format!("# (output truncated: {which})\n"));
        }
    }
    out
}

/// What a dying tab leaves behind for the reopen ring.
fn tab_death(tab: &Tab) -> (String, Option<std::path::PathBuf>) {
    (tab.title.lock().unwrap().clone(), tab.cwd_raw())
}

/// State of the cursor's movement pulse (see `App::ripple`).
struct Ripple {
    /// Centre in PHYSICAL pixels (the draw path works there; egui divides
    /// by the scale when painting).
    x: f32,
    y: f32,
    at: Instant,
    color: [f32; 3],
}

/// RGBA pixels -> PNG. The screenshot path; egui textures and glyphon
/// agree on RGBA, so this is a header plus a pass-through.
fn png_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = enc.write_header() {
            let _ = writer.write_image_data(rgba);
        }
    }
    out
}

/// Which index is active in a tab list after the tab at `removed` leaves
/// for another window: slots after it shift down; if it WAS the active
/// tab, the neighbour that slides into its place takes over; a list that
/// held one tab simply has none (the window closes).
fn move_active_follow(len: usize, active: usize, removed: usize) -> Option<usize> {
    if len <= 1 {
        return None;
    }
    if active > removed {
        Some(active - 1)
    } else if active < removed {
        Some(active)
    } else {
        Some(active.min(len - 2))
    }
}

fn swap_follow(a: usize, b: usize, active: usize) -> usize {
    if active == a {
        b
    } else if active == b {
        a
    } else {
        active
    }
}

fn egui_mods(m: ModifiersState) -> egui::Modifiers {
    let mut out = egui::Modifiers::NONE;
    if m.shift_key() {
        out |= egui::Modifiers::SHIFT;
    }
    if m.control_key() {
        out |= egui::Modifiers::CTRL;
    }
    if m.alt_key() {
        out |= egui::Modifiers::ALT;
    }
    if m.super_key() {
        out |= egui::Modifiers::COMMAND;
    }
    out
}

/// The named keys worth handing to egui from the FIFO hook; characters go
/// through `Event::Text` instead, so they never appear here.
fn egui_key_of(key: &Key) -> Option<egui::Key> {
    use winit::keyboard::NamedKey;
    Some(match key {
        Key::Named(NamedKey::Enter) => egui::Key::Enter,
        Key::Named(NamedKey::Escape) => egui::Key::Escape,
        Key::Named(NamedKey::Backspace) => egui::Key::Backspace,
        Key::Named(NamedKey::Delete) => egui::Key::Delete,
        Key::Named(NamedKey::Tab) => egui::Key::Tab,
        Key::Named(NamedKey::Home) => egui::Key::Home,
        Key::Named(NamedKey::End) => egui::Key::End,
        Key::Named(NamedKey::ArrowLeft) => egui::Key::ArrowLeft,
        Key::Named(NamedKey::ArrowRight) => egui::Key::ArrowRight,
        Key::Named(NamedKey::ArrowUp) => egui::Key::ArrowUp,
        Key::Named(NamedKey::ArrowDown) => egui::Key::ArrowDown,
        _ => return None,
    })
}

fn parse_key_spec(spec: &str) -> Option<(ModifiersState, Key)> {
    let mut mods = ModifiersState::empty();
    let mut keypart = "";
    for part in spec.split('+') {
        match part {
            "alt" => mods |= ModifiersState::ALT,
            "ctrl" | "control" => mods |= ModifiersState::CONTROL,
            "shift" => mods |= ModifiersState::SHIFT,
            "super" => mods |= ModifiersState::SUPER,
            other => keypart = other,
        }
    }
    let key = match keypart {
        "" => return None,
        "esc" | "escape" => Key::Named(NamedKey::Escape),
        "return" | "enter" => Key::Named(NamedKey::Enter),
        "tab" => Key::Named(NamedKey::Tab),
        "backspace" => Key::Named(NamedKey::Backspace),
        "delete" | "del" => Key::Named(NamedKey::Delete),
        "home" => Key::Named(NamedKey::Home),
        "end" => Key::Named(NamedKey::End),
        "pgup" => Key::Named(NamedKey::PageUp),
        "pgdn" | "pagedown" => Key::Named(NamedKey::PageDown),
        "up" => Key::Named(NamedKey::ArrowUp),
        "down" => Key::Named(NamedKey::ArrowDown),
        "left" => Key::Named(NamedKey::ArrowLeft),
        "right" => Key::Named(NamedKey::ArrowRight),
        "space" => Key::Named(NamedKey::Space),
        // "+" can't be written literally (it's the modifier separator).
        "plus" => Key::Character("+".into()),
        // "+" and "-" are also how the zoom bindings match the numpad
        // (NumLock on => winit delivers characters), so the harness can
        // reach enlarge/shrink with alt+shift+plus / alt+shift+minus.
        "minus" => Key::Character("-".into()),
        "braceleft" => Key::Character("{".into()),
        "braceright" => Key::Character("}".into()),
        "f1" => Key::Named(NamedKey::F1),
        "f2" => Key::Named(NamedKey::F2),
        "f3" => Key::Named(NamedKey::F3),
        "f4" => Key::Named(NamedKey::F4),
        "f5" => Key::Named(NamedKey::F5),
        "f6" => Key::Named(NamedKey::F6),
        "f7" => Key::Named(NamedKey::F7),
        "f8" => Key::Named(NamedKey::F8),
        "f9" => Key::Named(NamedKey::F9),
        "f10" => Key::Named(NamedKey::F10),
        "f11" => Key::Named(NamedKey::F11),
        "f12" => Key::Named(NamedKey::F12),
        c if c.chars().count() == 1 => Key::Character(c.into()),
        _ => return None,
    };
    Some((mods, key))
}

/// The terminal's answer to a key press, in the encoding legacy programs
/// expect (xterm's modified-keys conventions, DECCKM included):
///   * Ctrl+letter is the control byte; Alt prefixes ESC; both combine.
///   * Arrows/Home/End/PgUp/PgDn/Ins/Del/F-keys carry their modifiers as
///     `ESC [ 1 ; m X` (m = 1 + shift + 2*alt + 4*ctrl), which is what
///     vim, emacs, readline and friends parse. Dropping the modifier (the
///     old behaviour) made Ctrl+Right a plain Right and Alt+Enter a plain
///     Enter - the shell could not tell the chord had ever happened.
///   * DECCKM (application cursor keys, set by vim/tmux with `ESC [ ? 1 h`)
///     swaps the unmodified arrows/Home/End to the `ESC O X` forms.
/// A `None` here means genuinely nothing to send (unknown keys), not "app
/// shortcut" - those were matched by `find_action` upstream.
fn key_bytes(k: &Key, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();
    let esc = |body: &[u8]| -> Vec<u8> {
        let mut v = vec![0x1b];
        v.extend_from_slice(body);
        v
    };
    // xterm's modifier parameter: present only when something is actually
    // held - `ESC [ 1 ; 5 C` is Ctrl+Right, `ESC [ C` is plain Right, and
    // the difference is the only way the program can tell them apart.
    let mcode = 1 + shift as u8 + (alt as u8) * 2 + (ctrl as u8) * 4;
    // `ESC [ <n>;<m>~` for the numbered function keys.
    let csn = |n: u8| -> Vec<u8> {
        if mcode > 1 {
            esc(format!("[{n};{mcode}~").as_bytes())
        } else {
            esc(format!("[{n}~").as_bytes())
        }
    };
    // `ESC [ 1;<m> <fin>` for the lettered ones (arrows, Home/End, F1-F4).
    let csi1 = |fin: u8| -> Vec<u8> {
        if mcode > 1 {
            esc(format!("[1;{mcode}{fin}").as_bytes())
        } else {
            vec![0x1b, b'[', fin]
        }
    };
    match k {
        Key::Character(c) => {
            if ctrl {
                let byte = match c.to_ascii_lowercase().as_str() {
                    " " | "@" | "2" => Some(0x00u8),
                    "3" => Some(0x1b),
                    "6" => Some(0x1e),
                    "7" => Some(0x1f),
                    "8" => Some(0x7f),
                    "[" => Some(0x1b),
                    "\\" | "/" | "." | "," => Some(0x1c),
                    "]" => Some(0x1d),
                    "-" | "_" => Some(0x1f),
                    s if s.len() == 1 && s.as_bytes()[0].is_ascii_alphabetic() => {
                        Some(s.as_bytes()[0] & 0x1f)
                    }
                    // A control chord with no legacy control meaning: send
                    // the character plain rather than swallow the keypress.
                    // A dropped key is worse than a dropped modifier.
                    s if s.len() == 1 => Some(s.as_bytes()[0]),
                    s => {
                        return if s.is_empty() {
                            None
                        } else {
                            Some(if alt { esc(s.as_bytes()) } else { s.as_bytes().to_vec() })
                        }
                    }
                };
                let b = byte.unwrap();
                Some(if alt { vec![0x1b, b] } else { vec![b] })
            } else if alt {
                Some(esc(c.as_bytes()))
            } else {
                Some(c.as_bytes().to_vec())
            }
        }
        Key::Named(n) => Some(match n {
            // Space arrives here, not as Character(" "), whenever Ctrl or
            // Alt was involved - winit reports the chord as a named key,
            // and an old table that only knew plain space here silently
            // turned Ctrl+Space (the classic emacs set-mark) into a space.
            NamedKey::Space => {
                if ctrl {
                    vec![0x00]
                } else if alt {
                    vec![0x1b, 0x20]
                } else {
                    vec![0x20]
                }
            }
            NamedKey::Enter => {
                if alt {
                    vec![0x1b, 0x0d]
                } else {
                    vec![0x0d]
                }
            }
            // Ctrl+Backspace is C-h (xterm and VTE both send BS there);
            // Alt gets the meta prefix, and meta+C-h is readline's
            // backward-kill-word. Word-rubout proper remains Ctrl+W.
            NamedKey::Backspace => match (ctrl, alt) {
                (true, true) => vec![0x1b, 0x08],
                (true, false) => vec![0x08],
                (false, true) => vec![0x1b, 0x7f],
                (false, false) => vec![0x7f],
            },
            NamedKey::Tab => {
                if shift {
                    vec![0x1b, b'[', b'Z']
                } else if alt {
                    vec![0x1b, 0x09]
                } else {
                    vec![0x09]
                }
            }
            NamedKey::Escape => {
                if alt {
                    vec![0x1b, 0x1b]
                } else {
                    vec![0x1b]
                }
            }
            NamedKey::ArrowUp => cursor_key(b'A', mcode, app_cursor),
            NamedKey::ArrowDown => cursor_key(b'B', mcode, app_cursor),
            NamedKey::ArrowRight => cursor_key(b'C', mcode, app_cursor),
            NamedKey::ArrowLeft => cursor_key(b'D', mcode, app_cursor),
            NamedKey::Home => cursor_key(b'H', mcode, app_cursor),
            NamedKey::End => cursor_key(b'F', mcode, app_cursor),
            NamedKey::PageUp => csn(5),
            NamedKey::PageDown => csn(6),
            NamedKey::Delete => csn(3),
            NamedKey::Insert => csn(2),
            NamedKey::F1 => lettered_f(b'P', mcode),
            NamedKey::F2 => lettered_f(b'Q', mcode),
            NamedKey::F3 => lettered_f(b'R', mcode),
            NamedKey::F4 => lettered_f(b'S', mcode),
            NamedKey::F5 => csn(15),
            NamedKey::F6 => csn(17),
            NamedKey::F7 => csn(18),
            NamedKey::F8 => csn(19),
            NamedKey::F9 => csn(20),
            NamedKey::F10 => csn(21),
            NamedKey::F11 => csn(23),
            NamedKey::F12 => csn(24),
            _ => return None,
        }),
        _ => None,
    }
}

/// Arrows/Home/End: modified keys take the `ESC [ 1;m X` form; plain keys
/// take `ESC [ X`, or `ESC O X` while the program asked for application
/// cursor keys (DECCKM) - vim and less set that, and they read the two
/// forms differently.
fn cursor_key(fin: u8, mcode: u8, app_cursor: bool) -> Vec<u8> {
    if mcode > 1 {
        vec![0x1b, b'[', b'1', b';', b'0' + mcode, fin]
    } else if app_cursor {
        vec![0x1b, b'O', fin]
    } else {
        vec![0x1b, b'[', fin]
    }
}

/// F1-F4: `ESC O P` plain, `ESC [ 1;m P` with any modifier.
fn lettered_f(fin: u8, mcode: u8) -> Vec<u8> {
    if mcode > 1 {
        vec![0x1b, b'[', b'1', b';', b'0' + mcode, fin]
    } else {
        vec![0x1b, b'[', b'O', fin]
    }
}

/// The byte sequence of one mouse report in the protocol the terminal has
/// enabled. `code` is the xterm button code (0/1/2 press, 3 release, 32
/// motion, 64/65 wheel) already OR'd with the modifier bits (4 shift, 8 alt,
/// 16 ctrl); coordinates are 1-based viewport cells.
fn mouse_report_bytes(mode: TermMode, code: u16, col: u16, row: u16, release: bool) -> Vec<u8> {
    let tail = if release { b'm' } else { b'M' };
    if mode.contains(TermMode::SGR_MOUSE) {
        // SGR mode (?1006): CSI < Cb ; Cx ; Cy M/m - the leading < is what
        // distinguishes it, and parsers without it drop the report.
        format!("\x1b[<{};{};{}{}", code, col, row, tail as char).into_bytes()
    } else {
        // X10/normal mode: CSI M plus three binary bytes, each value +32.
        vec![0x1b, b'[', b'M', (code + 32).min(255) as u8, (col + 32).min(255) as u8, (row + 32).min(255) as u8]
    }
}

// ---------------------------------------------------------------------------
// ApplicationHandler
// ---------------------------------------------------------------------------

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.wins.is_empty() {
            self.make_window(event_loop, None, None, true);
        }
    }


    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.check_theme_files();
        self.drain_clipboard();
        self.drain_bell_pings();
        self.drain_sems();
        self.drain_apcs();
        self.drain_ptwrites();
        self.drain_resets();
        self.drain_cell_sizes();
        self.drain_test_keys(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Route first: every helper below means "the window this event is
        // for" via `cur`. Events for windows we do not know (one just
        // closed, or a stray) are dropped -- there is nothing correct left
        // to do with them.
        match self.wins.iter().position(|w| w.window.as_ref().is_some_and(|win| win.id() == id)) {
            Some(i) => self.cur = i,
            None => return,
        }
        let Some(window) = self.wins[self.cur].window.clone() else {
            return;
        };
        // Decide ownership BEFORE enqueueing the key. egui-winit always
        // consumes Tab and queues it for focus traversal, even with no editor
        // focused. Passing it through after enqueueing would let one Tab both
        // reach the terminal and focus a sidebar control on the next frame.
        let route_to_egui = match &event {
            WindowEvent::KeyboardInput { event: key, .. } => {
                let action = config::find_action(&self.config.bindings, &key.logical_key, self.modifiers, false);
                self.egui_owns_keyboard()
                    && !matches!(action.as_deref(), Some("show_config" | "show_history"))
            }
            _ => true,
        };
        // State and pointer events still reach egui regardless of keyboard ownership.
        let egui_consumed = route_to_egui && self.wins[self.cur].egui.as_mut()
            .is_some_and(|e| e.on_event(&window, &event));
        // A move and click can arrive before the next UI pass. With only
        // a sidebar open, use the actual event position so crossing its
        // edge neither swallows a terminal click nor leaks a panel click.
        let egui_consumed = match &event {
            WindowEvent::CursorMoved { .. } | WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. }
                if self.wins[self.cur].overview.is_none() => {
                self.wins[self.cur].egui.as_ref().filter(|e| {
                    let st = &e.ui_state;
                    (st.right_panel != egui_ui::RightPanel::Closed) && !st.menu_open && !st.move_open
                        && !st.rename_open && !st.font_popup.as_ref().is_some_and(|p| p.open)
                }).map(|e| {
                    let (x, y) = match &event {
                        WindowEvent::CursorMoved { position, .. } => (position.x as f32, position.y as f32),
                        _ => self.wins[self.cur].pointer,
                    };
                    let scale = e.ctx.pixels_per_point();
                    egui_ui::panel_pointer_input(&e.ctx, egui::pos2(x / scale, y / scale))
                }).unwrap_or(egui_consumed)
            }
            _ => egui_consumed,
        };
        // egui owns the strip's layout and drawing; native window tab drags
        // retain the pointer so a drop can cross the client/window boundary.
        let pointer_at = match &event {
            WindowEvent::CursorMoved { position, .. } => Some((position.x as f32, position.y as f32)),
            WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } => Some(self.wins[self.cur].pointer),
            _ => None,
        };
        let strip_pointer = pointer_at.is_some_and(|(_, y)| self.show_tab_strip() && y >= 0.0 && y < self.tab_h)
            && self.wins[self.cur].overview.is_none()
            && !self.wins[self.cur].egui.as_ref().is_some_and(|e| e.ui_state.menu_open || e.ui_state.move_open || e.ui_state.rename_open);
        let egui_consumed = if pointer_at.is_some() && (strip_pointer || self.wins[self.cur].tab_drag.is_some()
            || self.wins[self.cur].tab_close_press.is_some()) { false } else { egui_consumed };
        // An event egui claimed was claimed *for* a visual change -- hover,
        // press, drag feedback all only exist in the next frame. Every
        // motion over the panel is claimed, so without this the panel's
        // state would advance invisibly and only become visible when an
        // unrelated frame (the cursor-blink timer at best) happened to
        // repaint it -- the one-interaction-behind feel. winit coalesces
        // queued redraws, so a burst of events costs one frame.
        // ... and motion/click must ask for a frame even when egui has not
        // *yet* claimed the event: `consumed` is decided from egui's state
        // before it sees this event, so the very move that carries the
        // pointer onto a widget is unclaimed -- the frame that would render
        // the resulting hover is exactly the one that would go unscheduled.
        // Terminal input paths request their own frames already; this only
        // adds motion-paced redraws, which winit coalesces to one per frame.
        if egui_consumed
            || matches!(
                event,
                winit::event::WindowEvent::CursorMoved { .. }
                    | winit::event::WindowEvent::MouseInput { .. }
            )
        {
            window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => {
                // A window closes; its tabs die with it (they are pty
                // children and nowhere else to go). The session ends with
                // the last window, like the original's GApplication.
                self.close_window(self.cur, event_loop);
            },
            WindowEvent::RedrawRequested => {
                self.check_theme_files();
                self.drain_clipboard();
                self.drain_bell_pings();
                self.drain_sems();
                self.drain_apcs();
                self.drain_ptwrites();
                self.drain_resets();
                self.drain_cell_sizes();
                self.drain_test_keys(event_loop);
                self.drain_session_requests(event_loop);
                if let Some(action) = self.pending_action.take() {
                    self.do_action(&action, event_loop);
                }
                if let Some(target) = self.pending_move.take() {
                    let from = self.cur;
                    let idx = self.wins[from].active;
                    if target == usize::MAX {
                        self.make_window(event_loop, None, None, false);
                        let to = self.wins.len() - 1;
                        self.move_tab(from, idx, to, 0, event_loop);
                    } else if target < self.wins.len() && target != from {
                        let insert = self.wins[target].tabs.len();
                        self.move_tab(from, idx, target, insert, event_loop);
                    }
                }
                // Open the Alt-held switcher if the clock asked and Alt is
                // still held (guards against the press/release race).
                if self.switcher_show_req.swap(false, Ordering::SeqCst)
                    && (self.wins.len() == 1 || self.wins[self.cur].focused)
                    && self.switcher_alt_at.lock().unwrap().is_some()
                    && !self.wins[self.cur].tabs.is_empty()
                    && self.wins[self.cur].overview.map(|o| o.mode) != Some(OverviewMode::Switcher)
                    && !self.wins[self.cur].renaming
                {
                    self.wins[self.cur].overview = Some(OverviewState {
                        mode: OverviewMode::Switcher,
                        selected: self.wins[self.cur].active,
                        original: self.wins[self.cur].active,
                    });
                    if let Some(e) = self.wins[self.cur].egui.as_ref() {
                        egui_ui::release_focus(&e.ctx);
                    }
                    self.switcher_open.store(true, Ordering::SeqCst);
                }
                // Drain tabs whose child exited
                let mut to_close: Vec<usize> = Vec::new();
                {
                    let mut exited = self.exited.lock().unwrap();
                    if !exited.is_empty() {
                        to_close = exited.drain(..).collect();
                    }
                }
                // A window can die with these closes (the last tab closes
                // its window, the last window empties `wins`), and the
                // event loop still has queued work after `exit()`: every
                // `wins[self.cur]` below is guarded, or shutdown ends in a
                // panic on the second exited event instead of a clean exit.
                if !to_close.is_empty() {
                    // map stable ids back to current indices (ids are stable)
                    for id in to_close.iter().rev() {
                        if self.wins.is_empty() {
                            return;
                        }
                        if let Some(pos) = self.wins[self.cur].tabs.iter().position(|t| t.id == *id) {
                            self.close_tab_at(pos, event_loop);
                        }
                    }
                    if self.wins.is_empty() || self.wins[self.cur].tabs.is_empty() {
                        return;
                    }
                }
                // Draw unconditionally: winit only sends RedrawRequested when a
                // frame is actually wanted (our own request_redraw, resize, or
                // compositor damage). A hand-rolled damage flag here would have
                // to be remembered by every caller -- and silently swallow
                // frames for the ones that didn't (e.g. the cursor blink).
                self.draw(&window);
            },
            WindowEvent::CursorMoved { position, .. } => {
                let px = position.x as f32;
                let py = position.y as f32;
                // The pointer position is app state, not an action: keep it
                // current even while egui owns the pointer, so the next
                // unclaimed click hit-tests against the right cell.
                self.wins[self.cur].pointer = (px, py);
                if egui_consumed {
                    return;
                }
                // Terminal mouse reporting: all-motion mode reports every move,
                // drag mode only while a button is held. The protocol has no
                // sub-cell position, so one event per entered cell.
                {
                    let cell = self.mouse_cell(px, py);
                    if let (Some(mode), Some(cell)) = (self.mouse_mode(), cell) {
                        let motion = mode.contains(TermMode::MOUSE_MOTION)
                            || (mode.contains(TermMode::MOUSE_DRAG) && self.wins[self.cur].mouse_button.is_some());
                        if motion && self.wins[self.cur].mouse_last != Some(cell) {
                            self.wins[self.cur].mouse_last = Some(cell);
                            let code = match self.wins[self.cur].mouse_button {
                                Some(b) => self.mouse_button_code(b) | 32,
                                None => 32,
                            };
                            self.wins[self.cur].tabs[self.wins[self.cur].active].write(&mouse_report_bytes(mode, code, cell.0, cell.1, false));
                        }
                    }
                }
                let in_strip = self.show_tab_strip() && py < self.tab_h;
                let mut hover = usize::MAX;
                let mut hover_add = false;
                if in_strip {
                    for (i, (x0, x1)) in self.wins[self.cur].tab_hits.iter().enumerate() {
                        if px >= *x0 && px < *x1 {
                            hover = i;
                            break;
                        }
                    }
                    let (a0, a1) = self.wins[self.cur].add_hit;
                    hover_add = px >= a0 && px < a1;
                }
                // Drag reorder: the held tab trades places every time the
                // pointer crosses into another tab's cell. Swapping (rather
                // than shifting) keeps the neighbour order stable when the
                // pointer wobbles across a boundary.
                if let Some(from) = self.wins[self.cur].tab_drag {
                    if hover < self.wins[self.cur].tabs.len() && hover != from {
                        self.wins[self.cur].tabs.swap(from, hover);
                        self.wins[self.cur].active = swap_follow(from, hover, self.wins[self.cur].active);
                        self.wins[self.cur].tab_drag = Some(hover);
                        self.wins[self.cur].tab_drag_moved = true;
                        window.request_redraw();
                    }
                }
                if hover != self.wins[self.cur].hover_tab || hover_add != self.wins[self.cur].hover_add {
                    self.wins[self.cur].hover_tab = hover;
                    self.wins[self.cur].hover_add = hover_add;
                    window.request_redraw();
                }
                // Dragging extends the selection cell by cell: repainting on every
                // pixel step would spend a frame per mouse sample.
                if self.wins[self.cur].drag.is_some() {
                    if let Some((pt, side)) = self.cell_at(px, py) {
                        if self.wins[self.cur].drag != Some((pt, side)) {
                            self.wins[self.cur].drag = Some((pt, side));
                            let cur_active = self.wins[self.cur].active;
                            if let Some(tab) = self.wins[self.cur].tabs.get_mut(cur_active) {
                                tab.update_selection(pt, side);
                            }
                            window.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button, .. } => {
                if egui_consumed {
                    return;
                }
                // The app owns this click (terminal, tab strip): whoever held
                // egui keyboard focus is no longer who the user is typing to.
                if let Some(e) = self.wins[self.cur].egui.as_ref() {
                    egui_ui::release_focus(&e.ctx);
                }
                let at = self.wins[self.cur].pointer;
                let hit_tab = if self.show_tab_strip() && at.1 >= 0.0 && at.1 < self.tab_h {
                    self.wins[self.cur].tab_hits.iter().position(|&(left, right)| at.0 >= left && at.0 < right)
                } else { None };
                if button == MouseButton::Left {
                    if let Some(index) = hit_tab.filter(|&i| self.wins[self.cur].tab_close_hits.get(i)
                        .is_some_and(|rect| rect.contains(egui::pos2(at.0, at.1)))) {
                        self.wins[self.cur].tab_close_press = Some(self.wins[self.cur].tabs[index].id);
                        window.request_redraw();
                        return;
                    }
                }
                if button == MouseButton::Middle {
                    if let Some(index) = hit_tab {
                        self.close_tab_at(index, event_loop);
                        window.request_redraw();
                        return;
                    }
                }
                // While the app owns the mouse (and Shift is not bypassing it)
                // the click is the app's, not the selection's: no paste, no
                // word select, no tab-strip click.
                if matches!(button, MouseButton::Left | MouseButton::Middle | MouseButton::Right) {
                    if let (Some(cell), Some(mode)) = (self.mouse_cell(at.0, at.1), self.mouse_mode()) {
                        self.wins[self.cur].mouse_button = Some(button);
                        self.wins[self.cur].mouse_last = Some(cell);
                        self.wins[self.cur].tabs[self.wins[self.cur].active].write(&mouse_report_bytes(
                            mode,
                            self.mouse_button_code(button),
                            cell.0,
                            cell.1,
                            false,
                        ));
                        return;
                    }
                }
                if button == MouseButton::Right {
                    // The context menu: the app's own actions, offered to
                    // people who have not memorised the bindings. A right
                    // press the terminal wants to REPORT never reaches here
                    // (handled above), and one egui wants never reaches here
                    // either (egui_consumed) -- so vim and tmux keep their
                    // right button, exactly like every other one.
                    let (rows, seps) = self.menu_rows();
                    let (mx, my) = self.wins[self.cur].pointer;
                    if let Some(egui) = self.wins[self.cur].egui.as_mut() {
                        let st = &mut egui.ui_state;
                        st.menu_items = rows;
                        st.menu_seps = seps;
                        st.menu_pos = [mx, my];
                        st.menu_open = true;
                        st.menu_out = None;
                        st.menu_age = 0;
                    }
                    window.request_redraw();
                } else if button == MouseButton::Middle {
                    // Middle click pastes the primary selection -- the X11
                    // convention the original gets free from VTE.
                    if self.cell_at(at.0, at.1).is_some() { self.request_paste(clipboard::Kind::Primary); }
                } else if button == MouseButton::Left {
                    if let Some(index) = hit_tab {
                        // Pressing a tab arms a drag but does *not* activate
                        // it: switching on press yanks the view -- and its
                        // scrollback position -- out from under a reorder
                        // started from a scrolled-up terminal. The release
                        // decides whether this was a click.
                        self.wins[self.cur].drag = None;
                        self.wins[self.cur].tab_drag = Some(index);
                        self.wins[self.cur].tab_drag_moved = false;
                    } else if self.show_tab_strip() && at.1 >= 0.0 && at.1 < self.tab_h
                        && at.0 >= self.wins[self.cur].add_hit.0 && at.0 < self.wins[self.cur].add_hit.1 {
                        self.wins[self.cur].drag = None;
                        let cur_active = self.wins[self.cur].active;
                        if let Some(tab) = self.wins[self.cur].tabs.get_mut(cur_active) { tab.clear_selection(); }
                        self.add_tab();
                        window.request_redraw();
                    } else if button == MouseButton::Left
                        && self.url_at(at.0, at.1).is_some()
                        && (self.modifiers.control_key() || self.modifiers.shift_key())
                    {
                        // A modifier+click on a link belongs to the link,
                        // not to the grid: Ctrl opens it, Shift copies it,
                        // and neither starts a selection. That last part is
                        // not politeness - selection autosends to the
                        // clipboard, so a "copy the link" click that also
                        // word-selected would have its own copy overwritten
                        // by the word's text one moment later.
                        let url = self.url_at(at.0, at.1).unwrap();
                        if self.modifiers.control_key() {
                            Self::open_url(&url);
                        } else if let Some(cb) = self.clipboard.as_ref() {
                            if let Err(e) = cb.set(clipboard::Kind::Clipboard, url) {
                                log::warn!("copy url: {e}");
                            }
                        }
                    } else if let Some((pt, side)) = self.cell_at(at.0, at.1) {
                        // Alt+drag is a rectangular selection gesture. It must
                        // not open the Alt-held switcher halfway through a drag.
                        if self.modifiers.alt_key() {
                            *self.switcher_alt_at.lock().unwrap() = None;
                            self.switcher_show_req.store(false, Ordering::SeqCst);
                        }
                        // One click starts a run, two take the word, three take
                        // whole rows: what VTE does, and why the clicks have to be
                        // counted by hand.
                        let count = self.count_click(0, at);
                        let ty = match count {
                            _ if self.modifiers.alt_key() => SelectionType::Block,
                            3 => SelectionType::Lines,
                            2 => SelectionType::Semantic,
                            _ => SelectionType::Simple,
                        };
                        let cur_active = self.wins[self.cur].active;
                        if let Some(tab) = self.wins[self.cur].tabs.get_mut(cur_active) {
                            tab.begin_selection(pt, side, ty);
                        }
                        self.wins[self.cur].drag = Some((pt, side));
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state: ElementState::Released, button, .. } => {
                if button == MouseButton::Left {
                    if let Some(id) = self.wins[self.cur].tab_close_press.take() {
                        if let Some(index) = self.wins[self.cur].tabs.iter().position(|tab| tab.id == id) {
                            let (x, y) = self.wins[self.cur].pointer;
                            if self.wins[self.cur].tab_close_hits.get(index)
                                .is_some_and(|rect| rect.contains(egui::pos2(x, y))) {
                                self.close_tab_at(index, event_loop);
                            }
                        }
                        window.request_redraw();
                        return;
                    }
                }
                // A tab drag ends wherever the button comes up -- even over
                // egui, whose panels would otherwise swallow the release and
                // leave the drag armed. A release without a reorder was a
                // click: that is the moment the tab actually activates.
                if let Some(idx) = self.wins[self.cur].tab_drag.take() {
                    // Cross-window drop. X11 keeps reporting the pointer
                    // while it is outside the window, so a release can name
                    // one of our other windows (the tab moves there) or no
                    // window at all (the tab detaches into a new one).
                    // Wayland confines the pointer, so it never leaves and
                    // this branch never fires there; Alt+Shift+w with the
                    // picker is that gesture on Wayland.
                    let outside = self.wins[self.cur]
                        .window
                        .as_ref()
                        .and_then(|w| w.outer_position().ok().map(|_| w.outer_size()))
                        .is_some_and(|size| {
                            let (px, py) = self.wins[self.cur].pointer;
                            px < 0.0
                                || py < 0.0
                                || px >= size.width as f32
                                || py >= size.height as f32
                        });
                    if outside && idx < self.wins[self.cur].tabs.len() {
                        if let Some(win) = self.wins[self.cur].window.as_ref() {
                            if let Ok(at) = win.outer_position() {
                                let (px, py) = self.wins[self.cur].pointer;
                                let (gx, gy) = (at.x as f32 + px, at.y as f32 + py);
                                match self.win_at(gx, gy) {
                                    Some((to, insert)) if to != self.cur => {
                                        self.move_tab(self.cur, idx, to, insert, event_loop);
                                        return;
                                    }
                                    None => {
                                        self.make_window(event_loop, None, None, false);
                                        let to = self.wins.len() - 1;
                                        self.move_tab(self.cur, idx, to, 0, event_loop);
                                        return;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    if !self.wins[self.cur].tab_drag_moved && idx < self.wins[self.cur].tabs.len() && idx != self.wins[self.cur].active {
                        let cur_active = self.wins[self.cur].active;
                        if let Some(t) = self.wins[self.cur].tabs.get_mut(cur_active) {
                            t.clear_selection();
                        }
                        self.wins[self.cur].active = idx;
                        window.request_redraw();
                    }
                    self.wins[self.cur].tab_drag_moved = false;
                }
                if egui_consumed {
                    return;
                }
                // A reported release goes back to the app; the coordinates fall
                // back to the last reported cell when the pointer left the
                // terminal area mid-drag.
                if self.wins[self.cur].mouse_button.take().is_some() {
                    if let Some(mode) = self.mouse_mode() {
                        if let Some(cell) =
                            self.mouse_cell(self.wins[self.cur].pointer.0, self.wins[self.cur].pointer.1).or(self.wins[self.cur].mouse_last)
                        {
                            self.wins[self.cur].tabs[self.wins[self.cur].active].write(&mouse_report_bytes(
                                mode,
                                3 | self.mouse_mods(),
                                cell.0,
                                cell.1,
                                true,
                            ));
                        }
                    }
                    self.wins[self.cur].mouse_last = None;
                }
                if button == MouseButton::Left && self.wins[self.cur].drag.take().is_some() {
                    self.publish_primary();
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event: key, .. } => {
                if key.state == ElementState::Pressed {
                    log::info!(
                        "keydown logical={:?} text={:?} tracked={:?} consumed={}",
                        key.logical_key, key.text, self.modifiers, egui_consumed
                    );
                }
                if key.state == ElementState::Pressed {
                    let action = config::find_action(&self.config.bindings, &key.logical_key, self.modifiers, false);
                    // Panel shortcuts remain global while a sidebar field is focused.
                    // Other consumed presses belong only to egui's editor.
                    let panel_shortcut = matches!(action.as_deref(), Some("show_config" | "show_history"));
                    let repeatable = action.as_deref().is_none_or(|action| matches!(action,
                        "prev_tab" | "next_tab" | "prev_tab_row" | "next_tab_row" |
                        "scroll_page_up" | "scroll_page_down" | "enlarge_font" | "shrink_font"));
                    if (!egui_consumed || panel_shortcut) && (!key.repeat || repeatable) {
                        self.handle_key(&key.logical_key, Some(&key), event_loop);
                    }
                } else {
                    // Releases flow to the enhanced protocol only (see
                    // handle_key's early branch); cheaply, they no-op.
                    self.handle_key(&key.logical_key, Some(&key), event_loop);
                }
            },
            WindowEvent::MouseWheel { delta, .. } => {
                if egui_consumed {
                    return;
                }
                // Ctrl+wheel zooms the font, matching the original's GTK
                // accelerator. Checked before mouse reporting on purpose:
                // GTK gave the accelerator priority over the terminal too,
                // so vim cannot take the wheel while Ctrl is held.
                if self.modifiers.control_key() {
                    let z = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y as f64,
                        MouseScrollDelta::PixelDelta(p) => p.y,
                    };
                    if z != 0.0 {
                        self.zoom_term(z > 0.0);
                        window.request_redraw();
                        return;
                    }
                }
                // The wheel is part of the click protocol: while the app owns
                // the mouse it scrolls the app, not the scrollback. Positive
                // delta is the content moving down, i.e. wheel up.
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y,
                };
                if dy != 0.0 {
                    if let (Some(cell), Some(mode)) = (
                        self.mouse_cell(self.wins[self.cur].pointer.0, self.wins[self.cur].pointer.1),
                        self.mouse_mode(),
                    ) {
                        let code = (if dy > 0.0 { 64 } else { 65 }) | self.mouse_mods();
                        self.wins[self.cur].tabs[self.wins[self.cur].active].write(&mouse_report_bytes(mode, code, cell.0, cell.1, false));
                        return;
                    }
                }
                if self.cell_at(self.wins[self.cur].pointer.0, self.wins[self.cur].pointer.1).is_some() {
                    let lines = match delta {
                        MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as i32,
                        MouseScrollDelta::PixelDelta(p) => {
                            self.wins[self.cur].wheel_pixels += p.y as f32;
                            let lines = (self.wins[self.cur].wheel_pixels / self.cell_h) as i32;
                            self.wins[self.cur].wheel_pixels -= lines as f32 * self.cell_h;
                            lines
                        }
                    };
                    if let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) { tab.scroll(alacritty_terminal::grid::Scroll::Delta(lines)); }
                    window.request_redraw();
                }
            },
            WindowEvent::ModifiersChanged(m) => {
                let new = m.state();
                log::info!("mods changed -> {new:?}");
                let alt_up = new.alt_key() && !self.modifiers.alt_key();
                let alt_down = !new.alt_key() && self.modifiers.alt_key();
                // The ripple: a modifier - any modifier - being pressed
                // *bare* pulses the cursor. It answers "is my keyboard
                // landing HERE" at the only place that question is ever
                // asked: while typing, modifier state is invisible, and a
                // stuck or missed Ctrl is discovered late. A chord does not
                // ring (its keystroke already says where the cursor went);
                // typing does not ring at all.
                use winit::keyboard::ModifiersState as Ms;
                let old = self.modifiers;
                let fresh = (new.contains(Ms::CONTROL) && !old.contains(Ms::CONTROL))
                    || (new.contains(Ms::SHIFT) && !old.contains(Ms::SHIFT))
                    || (new.contains(Ms::ALT) && !old.contains(Ms::ALT))
                    || (new.contains(Ms::SUPER) && !old.contains(Ms::SUPER));
                // Adding a second modifier onto a held one is building a
                // chord, not asking "is my keyboard here" - no ring.
                let chorded = new.intersects(Ms::CONTROL | Ms::ALT | Ms::SUPER)
                    && old.intersects(Ms::CONTROL | Ms::ALT | Ms::SUPER);
                if fresh && !chorded {
                    if let Some(([x, y], color)) = self.cursor_pt {
                        self.ripple = Some(Ripple { x, y, at: Instant::now(), color });
                        window.request_redraw();
                    }
                }
                self.modifiers = new;
                if alt_up {
                    // Alt pressed: start the switcher hold timer. Not while
                    // the configuration window is open: Alt is a plain key
                    // in its text fields, and a held "Altica" must not raise
                    // the switcher behind the window.
                    if !self.wins[self.cur].egui.as_ref().is_some_and(|e| e.ui_state.panel_is(egui_ui::RightPanel::Settings)) {
                        *self.switcher_alt_at.lock().unwrap() = Some(std::time::Instant::now());
                    }
                    self.switcher_show_req.store(false, Ordering::SeqCst);
                } else if alt_down {
                    // Alt released: commit (active tab is already the pick) and
                    // dismiss the switcher if it's the Alt-held variant.
                    *self.switcher_alt_at.lock().unwrap() = None;
                    self.switcher_show_req.store(false, Ordering::SeqCst);
                    if self.switcher_open.load(Ordering::SeqCst) {
                        self.close_switcher();
                    }
                }
                window.request_redraw();
            },
            WindowEvent::Focused(focused) => {
                self.wins[self.cur].focused = focused;
                if focused {
                    // The user answered an attention request by focusing us;
                    // stop asking.
                    window.request_user_attention(None);
                }
                if let Some(tab) = self.wins[self.cur].tabs.get(self.wins[self.cur].active) {
                    tab.term.lock().is_focused = focused;
                }
                window.request_redraw();
            },
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.set_scale(scale_factor as f32);
                window.request_redraw();
            },
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.wins[self.cur].gpu.as_mut() {
                    if size.width > 0
                        && size.height > 0
                        && (size.width != gpu.config.width || size.height != gpu.config.height)
                    {
                        gpu.config.width = size.width;
                        gpu.config.height = size.height;
                        gpu.surface.configure(&gpu.device, &gpu.config);
                        gpu.viewport
                            .update(&gpu.queue, Resolution { width: size.width, height: size.height });
                    }
                }
                self.sync_term_size();
                window.request_redraw();
            },
            _ => {},
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // `wins` is routinely empty here: closing the last window empties it
        // before winit fires Exiting. And say goodbye to every window's
        // tabs, not just the current one's.
        for w in self.wins.iter_mut() {
            for tab in w.tabs.iter() {
                let _ = tab.sender.send(Msg::Shutdown);
            }
        }
    }
}

/// TEMPORARY compensation for the difference between our one alpha stage and the
/// original's, applied by default; `MYPIXELS_TRANSPARENCY` (or the deprecated
/// `MYPIXELS_OPACITY`) replaces it outright.
///
/// Two readings from the user, at the values actually used:
///   transparency 0.1 -> ours too dark (needs to be more see-through)
///   transparency 0.6 -> ours too transparent (needs to be less see-through)
/// The first wants eff > x, the second eff < x, so the correction must cross and
/// no constant or scale factor can do it. Modelled as a bump that ramps in over
/// the first tenth and a flatter slope above it:
///   eff = 0.75*x + 0.105*min(1, x/0.1)
/// which gives 0.18 at 0.1 and 0.555 at 0.6, is continuous, monotone, and 0 at 0
/// so an opaque config stays opaque. That is curve-fitting, not a derivation --
/// the likely real cause is that VTE composites its *text* inside the translucent
/// layer as well, which lifts the whole appearance rather than the background.
/// Re-check after the window/tint layer is modelled; then keep or delete this.
fn effective_transparency(cfg: f32) -> f32 {
    static OVERRIDE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    let over = *OVERRIDE.get_or_init(|| {
        std::env::var("MYPIXELS_TRANSPARENCY")
            .or_else(|_| std::env::var("MYPIXELS_OPACITY"))
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .map(|v| v.clamp(0.0, 1.0))
    });
    if let Some(v) = over {
        return v;
    }
    let x = cfg.clamp(0.0, 1.0);
    (0.75 * x + 0.105 * (x / 0.1).min(1.0)).clamp(0.0, 1.0)
}

/// Report what this compositor + driver pair will actually do with our alpha,
/// then exit. The window is created with `visible(false)`, so nothing is ever
/// mapped and the desktop is undisturbed -- which matters because the answer
/// differs per backend, per driver and per compositor, and cannot be settled by
/// reading their sources.
fn alpha_probe(event_loop: &EventLoop<()>) {
    let window = std::sync::Arc::new(
        event_loop
            .create_window(
                winit::window::Window::default_attributes()
                    .with_inner_size(winit::dpi::LogicalSize::new(64.0, 64.0))
                    .with_visible(false)
                    .with_transparent(true)
                    .with_title("alpha-probe"),
            )
            .expect("window"),
    );
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
        Box::new(event_loop.owned_display_handle()),
    ));
    let surface = instance.create_surface(window.clone()).expect("surface");
    for a in gpu::block_on(instance.enumerate_adapters(wgpu::Backends::all())) {
        let i = a.get_info();
        log::info!("probe: adapter available {:?} / {}", i.backend, i.name);
    }
    let adapter = gpu::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
        ..Default::default()
    }))
    .expect("adapter");
    let info = adapter.get_info();
    let caps = surface.get_capabilities(&adapter);
    log::info!("probe: chosen {:?} / {}", info.backend, info.name);
    log::info!("probe: alpha_modes advertised = {:?}", caps.alpha_modes);
    log::info!("probe: formats = {:?}", caps.formats);
    let (device, _queue) =
        gpu::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("device");
    // Some drivers advertise less than they accept, and vice versa: what decides
    // whether we get an ARGB buffer is whether configure() takes the mode.
    for m in [
        wgpu::CompositeAlphaMode::PreMultiplied,
        wgpu::CompositeAlphaMode::Inherit,
        wgpu::CompositeAlphaMode::PostMultiplied,
        wgpu::CompositeAlphaMode::Opaque,
    ] {
        let cfg = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: caps.formats[0],
            width: 64,
            height: 64,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: m,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            surface.configure(&device, &cfg);
        }))
        .is_ok();
        log::info!("probe: configure({m:?}) -> {}", if ok { "accepted" } else { "rejected" });
    }
    log::info!("probe: done");
}

/// Which commit this exact process was built from. Printed at startup and
/// by --version, because a single-instance session can outlive its own
/// binary and then argue about what it contains.
pub const BUILD: &str = env!("MYPIXELS_BUILD");

/// The stamp as a local clock string: build.rs carries epoch seconds
/// (constant-expression land has no formatter), we render it here.
pub fn build_stamp() -> String {
    let raw = BUILD;
    match raw.rsplit_once("built ") {
        Some((git, secs)) => {
            let secs: u64 = secs.parse().unwrap_or_default();
            let (h, m) = {
                let mut t = secs % 86400;
                if t < 0 { t += 86400; }
                (t / 3600 % 24, t / 60 % 60)
            };
            let d = secs / 86400;
            let (y, mo, da) = {
                let z = d + 719468;
                let era = z / 146097;
                let doe = z % 146097;
                let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
                let yy = yoe + era * 400;
                let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
                let mp = (5 * doy + 2) / 153;
                let day = doy - (153 * mp + 2) / 5 + 1;
                let mon = if mp < 10 { mp + 3 } else { mp - 9 };
                let year = if mon <= 2 { yy + 1 } else { yy };
                (year, mon, day)
            };
            format!("{git}, {y:04}-{mo:02}-{da:02} {h:02}:{m:02} UTC")
        }
        None => raw.to_string(),
    }
}

fn main() {
    if std::env::args().nth(1).is_some_and(|a| a == "--version" || a == "-V") {
        println!("mypixels, build {}", build_stamp());
        return;
    }

    env_logger::init();
    log::info!("mypixels build {}", build_stamp());
    let flags = leading_flags();
    if flags.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "usage: mypixels [options] [command args...]\n\n\
Options:\n  \
  -n, --new-window   open a new window instead of joining the session\n  \
  -s, --standalone   run a private instance: never join or host a session\n  \
  -h, --help         show this text\n\n\
  mypixels last [n] [--copy]
                   show (or clipboard-copy) the last n commands' output\
                   from the running session's memory pool\
\
Without flags, a mypixels window already running on this display is found\n\
over D-Bus and asked to open a tab for this command, in this directory.\n\
Typed inside a window, the tab opens in that same window. With no session\n\
to answer -- or with -n -- this process becomes a window of the session."
        );
        return;
    }
    // Daemon mode, client half: if the window this command was typed in (or,
    // outside any terminal, the session itself) is reachable, hand it the
    // command and exit. Anything the bus cannot answer -- no bus, no such
    // window, a session too old to speak this protocol -- means "become a
    // window yourself", never "lose the command". -n skips asking entirely:
    // an explicit second window.
    // `mypixels last [n] [--copy]`: the running session's memory pool of
    // command output, queried, not launched. Intercepted before the
    // open-a-tab path below, or "last" would spawn a program named last.
    {
        let terms = positional_terms();
        if terms.first().map(|s| s.as_str()) == Some("last") {
            let mut n: u64 = 1;
            let mut copy = false;
            for a in &terms[1..] {
                match a.parse::<u64>() {
                    Ok(v) if v >= 1 => n = v,
                    _ if a == "--copy" => copy = true,
                    _ => {
                        eprintln!("usage: mypixels last [n] [--copy]");
                        std::process::exit(2);
                    }
                }
            }
            match session::remote_last(n, copy) {
                Some(text) => {
                    if !copy {
                        print!("{text}");
                    }
                }
                None => {
                    eprintln!("mypixels last: no running session to ask");
                    std::process::exit(1);
                }
            }
            return;
        }
    }
    let standalone = flags.iter().any(|a| a == "--standalone" || a == "-s");
    if !standalone {
        let new_window = flags.iter().any(|a| a == "--new-window" || a == "-n");
        let cwd = std::env::current_dir().ok().map(|p| p.display().to_string());
        if session::remote_open(cwd, positional_terms(), new_window) {
            return;
        }
        // Nothing answered: a window of our own, session or not, is the
        // fallback -- with -n against a session too old to speak NewWindow,
        // that means a separate second process, the worst honest outcome.
    }
    // Running our own window from here. The shells this window spawns must
    // describe THIS window: a joiner's inherited $MYPIXELS_WINDOW would
    // point at the window we left, and --standalone must point at no
    // session at all (0). The bus thread overwrites 0 with the real slot
    // number in each tab's own environment (see Tab::new). Safe: no other
    // thread of this process exists yet.
    unsafe {
        if standalone {
            std::env::set_var("MYPIXELS_WINDOW", "0");
        } else {
            std::env::remove_var("MYPIXELS_WINDOW");
        }
    }
    let event_loop = EventLoop::new().expect("event loop");
    if std::env::args().any(|a| a == "--alpha-probe") {
        alpha_probe(&event_loop);
        return;
    }
    let mut app = App::new();
    event_loop.run_app(&mut app).expect("run");
    log::info!("exited");
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    fn run(row: u32, col: u32, text: &str) -> Run {
        Run {
            row,
            col,
            width: text.len() as u32,
            fg: [1.0, 1.0, 1.0],
            bg: [0.0, 0.0, 0.0],
            text: text.into(),
            cells: text.char_indices().enumerate().map(|(col, (byte, _))| (byte, col as u32)).collect(),
            attrs: Box::leak(Box::new(Attrs::new())),
        }
    }

    #[test]
    fn padding_is_not_shaped_but_background_width_is_preserved() {
        let mut runs = Vec::new();
        let mut padded = run(0, 0, "path.js      ");
        padded.bg = [0.2, 0.1, 0.0];
        push_run(&mut runs, padded, [1.0; 3], [0.0; 3]);
        assert_eq!(runs[0].text, "path.js");
        assert_eq!(runs[0].width, 13);
        assert_eq!(runs[0].cells.len(), 7);
    }

    #[test]
    fn invisible_padding_with_colored_foreground_is_discarded() {
        let mut runs = Vec::new();
        push_run(&mut runs, run(0, 0, "      "), [0.5; 3], [0.0; 3]);
        assert!(runs.is_empty());
    }

    #[test]
    fn signatures_see_text_cursor_and_default_colors() {
        let mut fd = FrameData {
            runs: vec![run(0, 0, "x")],
            cursor: None,
            ..FrameData::default()
        };
        let base = frame_sig(&fd, 7.0);
        fd.cursor = Some((1, 0));
        assert_ne!(base, frame_sig(&fd, 7.0), "the cursor is part of the frame");
        fd.cursor = None;
        fd.runs[0].text = "y".into();
        assert_ne!(base, frame_sig(&fd, 7.0), "so is the text");
        let old = frame_sig(&fd, 7.0);
        fd.bg = [1.0; 3];
        assert_ne!(old, frame_sig(&fd, 7.0), "blank previews also follow background changes");
        let again = frame_sig(&fd, 7.0);
        assert_eq!(again, frame_sig(&fd, 7.0), "same input, same signature");
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;
    use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

    #[test]
    fn osc_overrides_use_the_same_slots_for_named_and_indexed_colors() {
        let mut colors = alacritty_terminal::term::color::Colors::default();
        colors[1] = Some(Rgb { r: 24, g: 48, b: 72 });
        colors[17] = Some(Rgb { r: 12, g: 34, b: 56 });
        colors[256] = Some(Rgb { r: 255, g: 128, b: 0 });
        let palette = Palette::from_colors(&colors, &None);
        let resolve = |c| palette.resolve_with(&c, [0.8; 3], [0.1; 3], [0.4; 3]);
        assert_eq!(resolve(Color::Named(NamedColor::Red)), resolve(Color::Indexed(1)));
        assert_eq!(resolve(Color::Indexed(1)), config::Rgb(24, 48, 72).to_f32());
        assert_eq!(resolve(Color::Indexed(17)), config::Rgb(12, 34, 56).to_f32());
        assert_eq!(resolve(Color::Named(NamedColor::Foreground)), config::Rgb(255, 128, 0).to_f32());
        assert_eq!(resolve(Color::Named(NamedColor::Background)), [0.1; 3]);
        assert_eq!(resolve(Color::Named(NamedColor::Cursor)), [0.4; 3]);
    }

    #[test]
    fn theme_palette_leaves_the_256_color_cube_and_truecolor_intact() {
        let cfg = Config::from_text("theme: dracula", std::path::Path::new(".")).unwrap();
        let palette = Palette::from_colors(&Default::default(), &cfg.palette);
        let resolve = |c| palette.resolve_with(&c, [0.8; 3], [0.1; 3], [0.4; 3]);
        assert_eq!(resolve(Color::Indexed(1)), cfg.palette.unwrap()[1].to_f32());
        assert_eq!(resolve(Color::Indexed(196)), [1.0, 0.0, 0.0]);
        assert_eq!(resolve(Color::Indexed(17)), [0.0, 0.0, 95.0 / 255.0]);
        assert_eq!(resolve(Color::Indexed(232)), [8.0 / 255.0; 3]);
        assert_eq!(resolve(Color::Spec(Rgb { r: 1, g: 2, b: 3 })), config::Rgb(1, 2, 3).to_f32());
    }
}

#[cfg(test)]
mod mouse_report_tests {
    use super::*;

    fn mode_with(modes: &[TermMode]) -> TermMode {
        modes.iter().copied().fold(TermMode::NONE, |m, x| m | x)
    }

    #[test]
    fn x10_press_release_and_wheel() {
        let mode = mode_with(&[TermMode::MOUSE_REPORT_CLICK]);
        // Normal mode is CSI M plus binary bytes (value + 32), not text.
        assert_eq!(mouse_report_bytes(mode, 0, 42, 17, false), vec![0x1b, b'[', b'M', 32, 74, 49]);
        // release carries no tail in normal mode; the button code says it
        assert_eq!(mouse_report_bytes(mode, 3, 42, 17, true), vec![0x1b, b'[', b'M', 35, 74, 49]);
        assert_eq!(mouse_report_bytes(mode, 64, 1, 1, false), vec![0x1b, b'[', b'M', 96, 33, 33]);
        // ctrl + middle press: modifier bits are OR'd into the button code
        assert_eq!(mouse_report_bytes(mode, 1 | 16, 5, 6, false), vec![0x1b, b'[', b'M', 49, 37, 38]);
    }

    #[test]
    fn sgr_keeps_the_leading_bracket() {
        let mode = mode_with(&[TermMode::MOUSE_REPORT_CLICK, TermMode::SGR_MOUSE]);
        assert_eq!(String::from_utf8(mouse_report_bytes(mode, 0, 42, 17, false)).unwrap(), "\x1b[<0;42;17M");
        // motion with the left button held
        assert_eq!(String::from_utf8(mouse_report_bytes(mode, 0 | 32, 3, 9, false)).unwrap(), "\x1b[<32;3;9M");
        assert_eq!(String::from_utf8(mouse_report_bytes(mode, 3 | 8, 3, 9, true)).unwrap(), "\x1b[<11;3;9m");
        // wheel up and down
        assert_eq!(String::from_utf8(mouse_report_bytes(mode, 64, 10, 5, false)).unwrap(), "\x1b[<64;10;5M");
        assert_eq!(String::from_utf8(mouse_report_bytes(mode, 65, 10, 5, false)).unwrap(), "\x1b[<65;10;5M");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoscroll_direction_follows_the_wheel_sign() {
        // Drag near the top edge: up into history, Delta(+). Near the
        // bottom: toward the prompt, Delta(-). Middle: dead.
        let (dir, lines) = App::autoscroll_tick(10.0, 0.0, 400.0, 16.0).unwrap();
        assert_eq!((dir.signum(), lines >= 1), (1, true));
        let (dir, _) = App::autoscroll_tick(395.0, 0.0, 400.0, 16.0).unwrap();
        assert_eq!(dir.signum(), -1);
        assert!(App::autoscroll_tick(200.0, 0.0, 400.0, 16.0).is_none());
    }

    #[test]
    fn autoscroll_speeds_up_with_over_drag() {
        // Edge band: one line per tick. Each cell height pulled past the
        // edge: one more. A furious pull still moves a screen, not a book.
        let cap = 32;
        // band = 2.2 cells deep: grazing its far edge drifts one line...
        assert_eq!(App::autoscroll_tick(58.0, 24.0, 400.0, 16.0), Some((1, 1)));
        // ...mid-band travels two...
        assert_eq!(App::autoscroll_tick(32.0, 24.0, 400.0, 16.0), Some((2, 2)).map(|(a, b)| (1, b)));
        // ...and the grid edge itself (a full band of depth) three.
        assert_eq!(App::autoscroll_tick(24.0, 24.0, 400.0, 16.0), Some((1, 3)));
        // Past it (the strip zone, or the window's own border) keeps ramping.
        assert_eq!(App::autoscroll_tick(4.0, 24.0, 400.0, 16.0), Some((1, 4)));
        assert_eq!(App::autoscroll_tick(420.0, 0.0, 400.0, 16.0), Some((-1, 4)));
        assert_eq!(App::autoscroll_tick(-9000.0, 0.0, 400.0, 16.0).map(|(d, _)| d), Some(1));
        assert_eq!(App::autoscroll_tick(1.0, 0.0, 400.0, 16.0), Some((1, 3))); // near the border: three
        assert!(App::autoscroll_tick(-9000.0, 0.0, 400.0, 16.0).unwrap().1 <= cap);
    }

    fn kb(spec: &str, app_cursor: bool) -> Option<Vec<u8>> {
        let (mods, key) = parse_key_spec(spec).expect("spec");
        key_bytes(&key, mods, app_cursor)
    }

    #[test]
    fn modified_named_keys_carry_the_modifier_to_the_terminal() {
        // The reported breakage, as a truth table. Every one of these used
        // to be sent as the *plain* key - the program could not tell the
        // chord had ever happened.
        assert_eq!(kb("ctrl+right", false), Some(b"\x1b[1;5C".to_vec()));
        assert_eq!(kb("ctrl+left", false), Some(b"\x1b[1;5D".to_vec()));
        assert_eq!(kb("alt+right", false), Some(b"\x1b[1;3C".to_vec()));
        assert_eq!(kb("ctrl+shift+left", false), Some(b"\x1b[1;6D".to_vec()));
        assert_eq!(kb("ctrl+delete", false), Some(b"\x1b[3;5~".to_vec()));
        assert_eq!(kb("ctrl+home", false), Some(b"\x1b[1;5H".to_vec()));
        assert_eq!(kb("shift+tab", false), Some(b"\x1b[Z".to_vec()));
        assert_eq!(kb("ctrl+space", false), Some(vec![0x00]));
        assert_eq!(kb("alt+space", false), Some(vec![0x1b, 0x20]));
        assert_eq!(kb("ctrl+backspace", false), Some(vec![0x08]));
        assert_eq!(kb("alt+backspace", false), Some(vec![0x1b, 0x7f]));
        assert_eq!(kb("ctrl+alt+backspace", false), Some(vec![0x1b, 0x08]));
        assert_eq!(kb("ctrl+f5", false), Some(b"\x1b[15;5~".to_vec()));
        assert_eq!(kb("alt+f1", false), Some(b"\x1b[1;3P".to_vec()));
    }

    #[test]
    fn control_letters_and_meta_still_encode_classically() {
        assert_eq!(kb("ctrl+w", false), Some(vec![0x17]));
        assert_eq!(kb("ctrl+c", false), Some(vec![0x03]));
        assert_eq!(kb("ctrl+[", false), Some(vec![0x1b]));
        assert_eq!(kb("alt+f", false), Some(vec![0x1b, b'f']));
        // The digits that DO have control meanings in xterm.
        assert_eq!(kb("ctrl+2", false), Some(vec![0x00]));
        assert_eq!(kb("ctrl+6", false), Some(vec![0x1e]));
        assert_eq!(kb("ctrl+7", false), Some(vec![0x1f]));
        // A control chord with no legacy meaning sends the character
        // rather than swallowing the keystroke.
        assert_eq!(kb("ctrl+9", false), Some(vec![b'9']));
    }

    #[test]
    fn plain_keys_keep_their_traditional_bytes() {
        // The regression guard for the rewrite: plain keys must not pick
        // up modifier parameters, or every vim on the machine breaks.
        assert_eq!(kb("up", false), Some(b"\x1b[A".to_vec()));
        assert_eq!(kb("home", false), Some(b"\x1b[H".to_vec()));
        assert_eq!(kb("end", false), Some(b"\x1b[F".to_vec()));
        assert_eq!(kb("pgup", false), Some(b"\x1b[5~".to_vec()));
        assert_eq!(kb("delete", false), Some(b"\x1b[3~".to_vec()));
        assert_eq!(kb("backspace", false), Some(vec![0x7f]));
        assert_eq!(kb("tab", false), Some(vec![0x09]));
        assert_eq!(kb("return", false), Some(vec![0x0d]));
        assert_eq!(kb("f5", false), Some(b"\x1b[15~".to_vec()));
    }

    #[test]
    fn application_cursor_mode_switches_the_plain_cursor_keys() {
        // vim/tmux set DECCKM; the program then *reads* ESC O A where it
        // reads ESC [ A otherwise.
        assert_eq!(kb("up", true), Some(b"\x1bOA".to_vec()));
        assert_eq!(kb("home", true), Some(b"\x1bOH".to_vec()));
        assert_eq!(kb("end", true), Some(b"\x1bOF".to_vec()));
        // Modified keys never take the SS3 form, application mode or not.
        assert_eq!(kb("ctrl+up", true), Some(b"\x1b[1;5A".to_vec()));
    }

    #[test]
    fn png_rgba_round_trips_through_a_real_decoder() {
        // A quarter of the encoder's contract, proved against the decoder:
        // size, colour type, and exact pixels.
        let (w, h) = (4u32, 2u32);
        let mut rgba = Vec::new();
        for i in 0..(w * h) as usize {
            rgba.extend_from_slice(&[i as u8, 255 - i as u8, (i * 7) as u8, 255]);
        }
        let png = png_rgba(w, h, &rgba);
        assert!(!png.is_empty() && png[..8] == *b"\x89PNG\r\n\x1a\n");
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (w, h));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&buf[..rgba.len()], &rgba[..]);
    }

    #[test]
    fn a_moved_tabs_window_chooses_a_new_active() {
        assert_eq!(move_active_follow(1, 0, 0), None, "the window closes");
        assert_eq!(move_active_follow(3, 2, 2), Some(1), "active leaves: the neighbour before");
        assert_eq!(move_active_follow(3, 0, 2), Some(0), "another tab leaves: unaffected");
        assert_eq!(move_active_follow(3, 2, 0), Some(1), "a lower tab leaves: shift down");
        assert_eq!(move_active_follow(2, 1, 1), Some(0), "last index clamps after removal");
    }

    #[test]
    fn tiny_windows_do_not_autoscroll_on_a_parked_pointer() {
        // Two-cell grid: the bands would overlap; the middle must hold.
        assert!(App::autoscroll_tick(16.0, 0.0, 32.0, 16.0).is_none());
    }

    use super::swap_follow;

    #[test]
    fn dragging_a_tab_moves_the_active_index_with_it() {
        // Dragging the active tab: it follows to the drop cell.
        assert_eq!(swap_follow(0, 2, 0), 2);
        // Dragging another tab onto the active one: the active tab is
        // displaced to the drag's old cell (it is the same tab, so its
        // terminal -- and the selection inside it -- stay put).
        assert_eq!(swap_follow(2, 0, 0), 2);
        // A reorder that does not involve the active tab leaves it alone.
        assert_eq!(swap_follow(1, 2, 0), 0);
    }
}

#[cfg(test)]
mod url_tests {
    use super::url_spans_in_row;

    fn spans(line: &str) -> Vec<(usize, String)> {
        let chars: Vec<char> = line.chars().collect();
        url_spans_in_row(&chars, 0)
            .into_iter()
            .map(|s| (s.start, s.url))
            .collect()
    }

    #[test]
    fn finds_links_and_drops_prose_punctuation() {
        let line = "see https://example.com/a?b=c. Also http://x.org, ok";
        let got = spans(line);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].1, "http://x.org");
        assert_eq!(got[0].1, "https://example.com/a?b=c");
        assert_eq!(got[0].0, 4);
    }

    #[test]
    fn keeps_balanced_parens_and_peels_extra_ones() {
        let line = "https://en.wikipedia.org/wiki/Rust_(language)";
        assert_eq!(spans(line)[0].1, line);
        assert_eq!(spans("trailing close: https://a.io/x)")[0].1, "https://a.io/x");
    }

    #[test]
    fn ignores_schemeish_noise() {
        assert!(spans("go to https://here now").is_empty()); // host needs a dot
        assert!(spans("the http:// scheme").is_empty());
        assert!(spans("ftp://not.http").is_empty()); // http(s) only
        assert_eq!(spans("http://a.io").len(), 1);
    }

    /// The frame-budget guard. One row per redraw is the design; at 200
    /// columns it must cost microseconds, because a mouse move redraws and
    /// the terminal lock is not free. (Threshold is debug-build generous.)
    #[test]
    fn row_scan_is_fast() {
        let line = "curl -sS https://example.com/some/path?query=1&x=2 | jq .  ".repeat(4);
        let chars: Vec<char> = line.chars().collect();
        let n = 20_000;
        let t = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(url_spans_in_row(&chars, 0));
        }
        let per = t.elapsed() / n;
        eprintln!("{:?} per {}-char row", per, chars.len());
        assert!(per.as_micros() < 3000, "row scan too slow for a redraw path");
    }
}

#[cfg(test)]
mod sem_tests {
    use super::apply_sem_mark;
    use std::collections::VecDeque;

    #[test]
    fn marks_fold_into_blocks() {
        let mut b: VecDeque<super::CommandBlock> = VecDeque::new();
        apply_sem_mark(&mut b, 0, None, 10, 10, false, 1); // A
        apply_sem_mark(&mut b, 1, None, 10, 10, false, 1); // B
        apply_sem_mark(&mut b, 2, None, 11, 11, false, 1); // C
        apply_sem_mark(&mut b, 3, Some(0), 40, 40, false, 1); // D;0
        apply_sem_mark(&mut b, 0, None, 41, 41, false, 2); // A
        apply_sem_mark(&mut b, 3, Some(1), 55, 55, false, 2); // D;1
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].prompt_line, b[0].exit), (10, Some(0)));
        assert_eq!((b[1].prompt_line, b[1].exit), (41, Some(1)));
    }

    #[test]
    fn idle_prompt_is_not_a_running_command() {
        let mut blocks = VecDeque::new();
        apply_sem_mark(&mut blocks, 0, None, 0, 0, false, 1);
        apply_sem_mark(&mut blocks, 1, None, 0, 0, false, 1);
        assert!(!blocks[0].is_history_command());
        apply_sem_mark(&mut blocks, 2, None, 1, 1, false, 1);
        blocks[0].cmd_line = "printf hello".into();
        assert!(blocks[0].is_history_command());
        assert!(!blocks[0].done);
        apply_sem_mark(&mut blocks, 3, Some(0), 2, 2, false, 1);
        blocks[0].text = "a multiline prompt\n$ printf hello\nhello".into();
        super::finish_block(&mut blocks[0]);
        apply_sem_mark(&mut blocks, 0, None, 3, 3, false, 2);
        assert!(blocks[0].done);
        assert_eq!(blocks[0].cmd_line, "printf hello", "finishing output must keep the captured input");
        assert_eq!(blocks.iter().filter(|b| b.is_history_command()).count(), 1);
    }

    #[test]
    fn stray_and_missing_marks_survive() {
        let mut b: VecDeque<super::CommandBlock> = VecDeque::new();
        apply_sem_mark(&mut b, 3, Some(2), 5, 5, false, 0); // D without A: no block, no panic
        assert!(b.is_empty());
        apply_sem_mark(&mut b, 0, None, 6, 6, false, 1); // A
        apply_sem_mark(&mut b, 3, None, 9, 9, false, 1); // D with unknown status
        apply_sem_mark(&mut b, 3, Some(7), 9, 9, false, 1); // second D must not reopen D
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].exit, None); // first D won with "unknown"
    }

    #[test]
    fn ring_is_bounded() {
        let mut b: VecDeque<super::CommandBlock> = VecDeque::new();
        for i in 0..600 {
            apply_sem_mark(&mut b, 0, None, i, i, false, i as u64);
        }
        assert_eq!(b.len(), 512);
        assert_eq!(b.front().unwrap().prompt_line, 88);
    }

    fn block(text: &str, done: bool, seq: u64) -> super::CommandBlock {
        let mut q: VecDeque<super::CommandBlock> = VecDeque::new();
        apply_sem_mark(&mut q, 0, None, 0, 0, false, seq);
        let mut b = q.pop_front().unwrap();
        b.text = text.to_string();
        b.done = done;
        b
    }

    #[test]
    fn block_cap_keeps_the_tail() {
        let mut b = block("hello ", false, 1);
        super::push_block_text(&mut b, &"x".repeat(500), 100);
        assert!(b.text.len() < 200, "clipped to roughly the cap");
        assert!(b.text.ends_with('x'), "the tail survives");
        assert!(b.truncated_tail);
        assert!(b.text.starts_with("[... 456 bytes omitted ...]"));
    }

    #[test]
    fn block_cap_cuts_on_char_boundaries() {
        let mut b = block("", false, 1);
        // The keep-window may slice a multi-byte char in half.
        super::push_block_text(&mut b, &"é".repeat(200), 50);
        assert!(b.text.is_char_boundary(b.text.len()));
    }

    #[test]
    fn closed_block_compresses_and_round_trips() {
        let original = "line of build output\n".repeat(200);
        let mut b = block(&original, true, 1);
        super::finish_block(&mut b);
        assert!(b.compressed.is_some(), "repetitive text compresses");
        assert!(b.text.is_empty(), "the live buffer gives way to the codec");
        assert!(b.stored_len() < original.len() / 2, "the budget sees the compressed size");
        assert_eq!(b.readable_text(), original, "the query path gets the exact bytes back");
        assert!(b.compressed.is_none(), "a second query pays nothing");
        // Small blocks are not worth a frame header.
        let mut s = block("tiny", true, 2);
        super::finish_block(&mut s);
        assert!(s.compressed.is_none());
        assert_eq!(s.readable_text(), "tiny");
    }

    #[test]
    fn budget_evicts_oldest_closed_first() {
        let mut map: std::collections::HashMap<usize, VecDeque<super::CommandBlock>> =
            std::collections::HashMap::new();
        // Tab 9: three closed blocks; tab 4: one closed and one open.
        map.insert(9, vec![block("aaaa", true, 1), block("bbbb", true, 2), block("cccc", true, 3)].into());
        map.insert(4, vec![block("dddd", true, 4), block("eeee", false, 5)].into());
        // Budget for exactly two 4-byte blocks.
        super::evict_to_budget(&mut map, 8);
        assert_eq!(map[&9].len(), 0, "global LRU: all three of tab 9 predate tab 4's");
        assert_eq!(map[&4].len(), 2, "the open block is sacred and fills the budget exactly");
        // An impossible budget strips everything that can be stripped.
        super::evict_to_budget(&mut map, 0);
        assert_eq!(map[&4].len(), 1);
        assert!(!map[&4][0].done, "the open block survives any budget");
    }

    #[test]
    fn all_open_over_budget_is_tolerated() {
        let mut map: std::collections::HashMap<usize, VecDeque<super::CommandBlock>> =
            std::collections::HashMap::new();
        map.insert(1, vec![block("aaaa", false, 1), block("bbbb", false, 2)].into());
        super::evict_to_budget(&mut map, 1); // nothing is closed
        assert_eq!(map[&1].len(), 2, "running commands are sacred, over budget or not");
    }

    #[test]
    fn last_one_is_bare_pasteable_text() {
        let mut q: VecDeque<super::CommandBlock> = VecDeque::new();
        apply_sem_mark(&mut q, 0, None, 0, 0, false, 1);
        apply_sem_mark(&mut q, 3, Some(0), 4, 4, false, 1);
        q.back_mut().unwrap().text = "total 42\nREADME.md\n".into();
        let out = super::format_last_output(&mut q, 1);
        assert_eq!(out, "total 42\nREADME.md\n", "one block pipes like a file, headers would corrupt it");
    }

    #[test]
    fn last_many_navigates_with_headers() {
        let mut q: VecDeque<super::CommandBlock> = VecDeque::new();
        for (i, (exit, text)) in [(Some(2), "one\n"), (Some(0), "two\n")].iter().enumerate() {
            apply_sem_mark(&mut q, 0, None, i, i, false, i as u64 + 1);
            apply_sem_mark(&mut q, 3, *exit, i + 1, i + 1, false, i as u64 + 1);
            q.back_mut().unwrap().text = text.to_string();
        }
        let out = super::format_last_output(&mut q, 5);
        assert!(out.contains("1 of 2 · exit 2"), "oldest-numbered first, failing command visible: {out}");
        assert!(out.contains("2 of 2 · ok"));
        // Chronological: newest last, beside the prompt the user reads next.
        assert!(out.find("one").unwrap() < out.find("two").unwrap());
    }

    #[test]
    fn the_running_query_command_yields_its_slot() {
        // `mypixels last 1 --copy` runs AS a command: its own block is
        // open and textless. The answer must be the last finished
        // command, not an empty copy.
        let mut q: VecDeque<super::CommandBlock> = VecDeque::new();
        apply_sem_mark(&mut q, 0, None, 0, 0, false, 1);
        apply_sem_mark(&mut q, 3, Some(0), 4, 4, false, 1);
        q.back_mut().unwrap().text = "echoed\n".into();
        apply_sem_mark(&mut q, 0, None, 5, 5, false, 2); // the query itself
        let out = super::format_last_output(&mut q, 1);
        assert_eq!(out, "echoed\n", "the live query block is skipped, not served");
    }

    #[test]
    fn last_reports_what_cannot_be_shown() {
        let mut q: VecDeque<super::CommandBlock> = VecDeque::new();
        // A full-screen program and a head-truncated giant.
        apply_sem_mark(&mut q, 0, None, 0, 0, true, 1);
        apply_sem_mark(&mut q, 3, Some(0), 4, 4, false, 1);
        apply_sem_mark(&mut q, 0, None, 5, 5, false, 2);
        apply_sem_mark(&mut q, 3, Some(0), 9, 9, false, 2);
        q.back_mut().unwrap().truncated_head = true;
        let out = super::format_last_output(&mut q, 2);
        assert!(out.contains("full-screen program"));
        assert!(out.contains("truncated: head"));
    }
}
