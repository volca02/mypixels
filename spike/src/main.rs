//! mypixels (Rust port): tabbed terminal.
//! alacritty_terminal (emulation + pty) rendered with glyphon (cosmic_text)
//! on wgpu in a winit window.
//!
//! Usage: mypixels-rs [command [args...]]   (defaults to $SHELL or bash)

mod config;
mod fonts;
mod gpu;
mod tab;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::Msg;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;

use glyphon::{
    Attrs, Buffer, Family, FontSystem, Metrics, Resolution, Shaping, TextArea, TextBounds,
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use config::Config;
use gpu::Gpu;
use tab::{SpikeListener, Tab};

const HISTORY: usize = 10_000;
const MAX_RUNS: usize = 4096;
const TAB_PAD: f32 = 8.0;
/// Gap between tabs -- the original gets it from `margin-right: 1px`.
const TAB_GAP: f32 = 1.0;
/// The original's `clock()` prepend stamps HH:MM in this colour.
const CLOCK_COLOR: [f32; 3] = [0.6, 0.4, 0.4];
/// Vertical padding inside a tab. The original gets 21px for Terminus 12 from
/// GTK (`padding: 2px 8px` plus 2px label margins); we reproduce the 2+2.
const TAB_VPAD: f32 = 2.0;

const PALETTE: [[f32; 3]; 16] = [
    [0.0, 0.0, 0.0], [0.8, 0.2, 0.2], [0.2, 0.6, 0.2], [0.6, 0.6, 0.2],
    [0.2, 0.2, 0.8], [0.6, 0.2, 0.6], [0.2, 0.6, 0.6], [0.8, 0.8, 0.8],
    [0.4, 0.4, 0.4], [1.0, 0.4, 0.4], [0.4, 1.0, 0.4], [1.0, 1.0, 0.4],
    [0.4, 0.4, 1.0], [1.0, 0.4, 1.0], [0.4, 1.0, 1.0], [1.0, 1.0, 1.0],
];


fn cube_color(i: u8) -> [f32; 3] {
    let i = (i - 16) as usize;
    let (r, g, b) = (i / 36, (i / 6) % 6, i % 6);
    let c = |v: usize| if v == 0 { 0.0 } else { (95.0 + 40.0 * v as f32) / 255.0 };
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
    fn resolve_with(&self, c: &alacritty_terminal::vte::ansi::Color, fg: [f32; 3], bg: [f32; 3]) -> [f32; 3] {
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
                    CURSOR_COLOR
                } else {
                    PALETTE[i % 16]
                }
            },
            Color::Indexed(i) => {
                let j = 16 + *i as usize;
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

const CURSOR_COLOR: [f32; 3] = [0.55, 0.55, 0.6];

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
/// (`SPIKE_NOCACHE=1`).
fn shaping_cache_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SPIKE_NOCACHE").is_none())
}

/// Append a run, dropping the ones that could never draw anything: padding in
/// the default colours. A mostly-empty 4K grid is 80k cells, and this is the
/// difference between thousands of dead runs and the handful that matter. Runs
/// carrying a background colour are kept -- that is how a TUI's highlighted row
/// still shows.
fn push_run(runs: &mut Vec<Run>, r: Run, def_fg: [f32; 3], def_bg: [f32; 3]) {
    if r.fg == def_fg && r.bg == def_bg && r.text.chars().all(|c| c == ' ') {
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
    /// Font for this run. Runs split where the required face changes, so a
    /// starship prompt becomes "text | icon | text" instead of "text | tofu".
    attrs: &'static Attrs<'static>,
}

struct FrameData {
    runs: Vec<Run>,
    cursor: Option<(u32, u32)>,
}

/// Tab overview navigation state (mirrors the Python TabOverview, standalone
/// mode). `selected` is the highlighted tile; `desired_column` is remembered
/// so up/down navigation stays in a column even when rows have fewer tabs.
/// Which overview variant is on screen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OverviewMode {
    /// Full control grid (Alt+o / `show_tab_overview`): nav, move, new, close.
    Standalone,
    /// Alt-held popup: follows the active tab, dismissed on Alt release.
    Switcher,
}

/// Overview navigation state. For `Standalone`, `selected`/`desired_column`
/// are the highlighted tile. For `Switcher`, the selection is the active tab
/// (`selected` mirrors it) and `desired_column` is kept for vertical nav.
#[derive(Clone, Copy)]
struct OverviewState {
    mode: OverviewMode,
    selected: usize,
    desired_column: usize,
}

/// Grid columns for a tab overview, matching the original:
/// `ceil(sqrt(n * 1.5))`, at least 1.
fn grid_cols(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    ((n as f32 * 1.5).sqrt().ceil() as usize).max(1)
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct App {
    config: Config,
    tabs: Vec<Tab>,
    active: usize,
    next_id: usize,
    dims: Dims,
    cols: usize,
    rows: usize,
    cell_w: f32,
    cell_h: f32,
    tab_char_w: f32,
    tab_h: f32,
    tab_text_h: f32,
    /// Vertical metrics of the term/tablist fonts, read from the fonts
    /// themselves -- line height differs a lot per family (Cascadia 9pt lands on
    /// 15px, Terminus 12 on 17px) and one guessed factor fit neither.
    term_metrics: fonts::LineMetrics,
    tab_metrics: fonts::LineMetrics,
    /// Strip hit boxes from the last drawn frame, in window pixels: (x0, x1)
    /// per tab, plus the [+] button. Recorded while drawing, read by the mouse
    /// handlers, so hit-testing can never disagree with what is on screen.
    tab_hits: Vec<(f32, f32)>,
    add_hit: (f32, f32),
    /// Tab under the pointer, or usize::MAX for none.
    hover_tab: usize,
    hover_add: bool,
    tablist_hidden: bool,
    fg: [f32; 3],
    bg: [f32; 3],
    exited: Arc<Mutex<Vec<usize>>>,
    window_slot: Arc<Mutex<Option<Arc<Window>>>>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    font_system: FontSystem,
    mono_attrs: Attrs<'static>,
    tab_attrs: Attrs<'static>,
    /// Per-codepoint font routing, rebuilt whenever the terminal font changes.
    coverage: RefCell<Option<fonts::Coverage>>,
    /// Runtime terminal font state. `term_pt` is the live size (enlarge/
    /// shrink font mutates it); a config reload resets it to the config value.
    term_family: &'static str,
    term_pt: f32,
    /// Inline rename mode: when true, keys edit `rename_draft` instead of the
    /// terminal; Enter commits, Esc cancels.
    renaming: bool,
    rename_draft: String,
    /// Tab overview (standalone mode): when Some, the overview overlay is
    /// shown and keys navigate/select/move tabs instead of reaching the
    /// terminal. `selected` is the highlighted tab; `desired_column` is kept
    /// for vertical navigation (like the original TabOverview).
    overview: Option<OverviewState>,
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
    modifiers: ModifiersState,
    program: String,
    args: Vec<String>,
    /// Test hook: receiver of key specs fed from the $SPIKE_KEYS FIFO by a
    /// reader thread (about_to_wait is rarely called on Wayland).
    key_rx: Option<std::sync::mpsc::Receiver<String>>,
}

impl App {
    fn new() -> Self {
        let config = Config::load();
        // Pango sizes are points; cosmic-text wants pixels (@96 dpi).
        let (term_family, term_pt) = config::parse_font(&config.term_font);
        let (tab_family, tab_pt) = config::parse_font(&config.tablist_font);
        // Fallback chain must know the primary families up front, so powerline /
        // Nerd Font glyphs in prompts resolve to icons instead of tofu.
        let (mut font_system, fallback_chain) =
            fonts::build_font_system(&[term_family.as_str(), tab_family.as_str()]);
        log::info!("font fallback chain: {:?}", fallback_chain);
        let font_px = term_pt * 96.0 / 72.0;
        let tab_font_px = tab_pt * 96.0 / 72.0;
        // Font size comes from the Buffer Metrics (see Gpu::init), not Attrs.
        // mono_attrs/tab_attrs are 'static, so the family names must be too.
        let term_family: &'static str = Box::leak(term_family.into_boxed_str());
        let tab_family: &'static str = Box::leak(tab_family.into_boxed_str());
        let (mono_attrs, tab_attrs) = {
            let db = font_system.db();
            (
                fonts::pin_face(
                    db,
                    Attrs::new().family(Family::Name(term_family)),
                    term_family,
                ),
                fonts::pin_face(
                    db,
                    Attrs::new().family(Family::Name(tab_family)),
                    tab_family,
                ),
            )
        };

        // Line height comes from the font itself, not a guessed factor.
        let term_metrics =
            fonts::line_metrics(&mut font_system, &term_family).unwrap_or(fonts::LineMetrics::DEFAULT);
        let tab_metrics =
            fonts::line_metrics(&mut font_system, &tab_family).unwrap_or(fonts::LineMetrics::DEFAULT);
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
        let tab_h = (tab_lh + TAB_VPAD * 2.0).round();

        log::info!(
            "font: term {term_family} {term_pt}pt, tablist {tab_family} {tab_pt}pt; cell = {cell_w:.2} x {cell_h:.2}, tab char = {tab_char_w:.2}, strip = {tab_h:.1}px"
        );

        let fg = config.foreground_color.to_f32();
        let bg = config.background_color.to_f32();

        let args: Vec<String> = std::env::args().collect();
        let (program, args) = if args.len() > 1 {
            (args[1].clone(), args[2..].to_vec())
        } else {
            (std::env::var("SHELL").unwrap_or_else(|_| "bash".into()), vec![])
        };

        let window_slot = Arc::new(Mutex::new(None));
        let key_rx = spawn_key_reader(Arc::clone(&window_slot));
        let shared = SpikeListener {
            window: Arc::clone(&window_slot),
            tab_id: 0,
            title: Arc::new(Mutex::new(String::new())),
            exited: Arc::new(Mutex::new(Vec::new())),
            last_activity: Arc::new(Mutex::new(std::time::Instant::now())),
        };
        let exited = Arc::clone(&shared.exited);
        let last_activity = Arc::clone(&shared.last_activity);
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
                        if let Some(w) = ws.lock().unwrap().as_ref() {
                            w.request_redraw();
                        }
                    }
                    if !open.load(Ordering::SeqCst) {
                        // Open after the configured Alt hold delay.
                        if let Some(at) = *alt_at.lock().unwrap() {
                            if now.duration_since(at).as_millis() >= delay_ms as u128 {
                                show_req.store(true, Ordering::SeqCst);
                            }
                        }
                    } else if now.duration_since(last_refresh).as_millis() >= refresh_ms as u128 {
                        // Keep every preview live while the switcher is open.
                        last_refresh = now;
                        if let Some(w) = ws.lock().unwrap().as_ref() {
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
        let dims = Dims { cols, rows };
        let tablist_hidden = config.tablist_hide;

        App {
            config,
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            dims,
            cols,
            rows,
            cell_w,
            cell_h,
            tab_char_w,
            tab_h,
            tab_text_h: tab_lh,
            term_metrics,
            tab_metrics,
            tab_hits: Vec::new(),
            add_hit: (0.0, 0.0),
            hover_tab: usize::MAX,
            hover_add: false,
            tablist_hidden,
            fg,
            bg,
            exited,
            window_slot,
            window: None,
            gpu: None,
            font_system,
            mono_attrs,
            tab_attrs,
            coverage: RefCell::new(None),
            term_family,
            term_pt,
            renaming: false,
            rename_draft: String::new(),
            overview: None,
            switcher_alt_at,
            switcher_show_req,
            switcher_open,
            cursor_blink,
            last_activity,
            modifiers: ModifiersState::empty(),
            program,
            args,
            key_rx,
        }
    }

    fn init_gpu(&mut self, event_loop: &ActiveEventLoop, window: &Arc<Window>) {
        // Live sizes, not the config's: `term_pt` carries any zoom applied this
        // session, and cell_h/tab_text_h are the rounded line heights we lay
        // the grid out with, so the glyph buffers must match them exactly.
        let term_px = self.term_pt * 96.0 / 72.0;
        let (_, tab_pt) = config::parse_font(&self.config.tablist_font);
        let tab_px = tab_pt * 96.0 / 72.0;
        self.gpu = Some(Gpu::init(
            event_loop,
            window,
            &mut self.font_system,
            term_px,
            self.cell_h,
            tab_px,
            self.tab_text_h,
        ));
    }

    fn add_tab(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        let window_size = WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.cols as u16,
            cell_width: self.cell_w as u16,
            cell_height: self.cell_h as u16,
        };
        let tab = Tab::new(
            id,
            &self.dims,
            window_size,
            self.program.clone(),
            self.args.clone(),
            &self.listener(),
            self.config.scrollback_lines,
        );
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    fn listener(&self) -> SpikeListener {
        SpikeListener {
            window: Arc::clone(&self.window_slot),
            tab_id: 0,
            title: Arc::new(Mutex::new(String::new())),
            exited: Arc::clone(&self.exited),
            last_activity: Arc::clone(&self.last_activity),
        }
    }

    fn close_tab_at(&mut self, idx: usize, event_loop: &ActiveEventLoop) {
        if idx >= self.tabs.len() {
            return;
        }
        // Stable ids: find the id of the tab being removed
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            event_loop.exit();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > idx {
            self.active -= 1;
        }
    }

    /// Whether the tab strip is currently shown (manual hide flag and
    /// `hide_single_tab` both apply).
    fn show_tab_strip(&self) -> bool {
        !self.tablist_hidden && !(self.config.hide_single_tab && self.tabs.len() <= 1)
    }

    /// Resize terminal grid of all tabs to fit the current surface size.
    fn sync_term_size(&mut self) {
        let (w, h) = match self.gpu.as_ref() {
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
        if let Ok(g) = std::env::var("SPIKE_GRID") {
            if let Some((c, r)) = g.split_once('x') {
                ncols = c.parse().unwrap_or(ncols);
                nrows = r.parse().unwrap_or(nrows);
            }
        }
        if (ncols, nrows) != (self.cols, self.rows) {
            eprintln!("term resize: {}x{} -> {}x{}", self.cols, self.rows, ncols, nrows);
            self.cols = ncols;
            self.rows = nrows;
            self.dims.cols = ncols;
            self.dims.rows = nrows;
            let window_size = WindowSize {
                num_lines: nrows as u16,
                num_cols: ncols as u16,
                cell_width: self.cell_w as u16,
                cell_height: self.cell_h as u16,
            };
            for tab in &self.tabs {
                tab.term.lock().resize(self.dims);
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
        self.bg = self.config.background_color.to_f32();
        if font_changed {
            self.apply_fonts();
        }
    }

    /// Rebuild the terminal font from the live state (`term_family` +
    /// `term_pt`): attrs, cell metrics, the term glyph pool, and the grid.
    /// The base Metrics are baked into each pooled Buffer, so a change needs
    /// new buffers.
    fn rebuild_term_font(&mut self) {
        let font_px = self.term_pt * 96.0 / 72.0;
        self.term_metrics = fonts::line_metrics(&mut self.font_system, self.term_family)
            .unwrap_or(fonts::LineMetrics::DEFAULT);
        let term_lh = self.term_metrics.height_at(font_px);
        self.mono_attrs = {
            let db = self.font_system.db();
            fonts::pin_face(
                db,
                Attrs::new().family(Family::Name(self.term_family)),
                self.term_family,
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

        if let Some(gpu) = &mut self.gpu {
            gpu.pool = (0..MAX_RUNS)
                .map(|_| Buffer::new(&mut self.font_system, Metrics::new(font_px, term_lh)))
                .collect();
            gpu.prompt_buf =
                Buffer::new(&mut self.font_system, Metrics::new(font_px, term_lh));
            // New buffers hold nothing, so every cached signature is a lie.
            gpu.slot_sig = vec![0u64; gpu.pool.len()];
        }
        // Cell size changed -> the grid that fits the surface changes.
        self.sync_term_size();
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
    fn cell_attrs(&self, ch: char) -> &'static Attrs<'static> {
        let mut cov = self.coverage.borrow_mut();
        let cov = cov.get_or_insert_with(|| {
            fonts::Coverage::new(
                &self.font_system,
                &self.mono_attrs,
                &[self.term_family.to_string()],
            )
        });
        cov.attrs_for(self.font_system.db(), ch)
    }

    /// Rebuild the tablist font from config (family + size): attrs, tab
    /// metrics, and the tab glyph pool.
    fn rebuild_tab_font(&mut self) {
        let (tab_family, tab_pt) = config::parse_font(&self.config.tablist_font);
        let tab_font_px = tab_pt * 96.0 / 72.0;
        let tab_family: &'static str = Box::leak(tab_family.into_boxed_str());
        self.tab_metrics = fonts::line_metrics(&mut self.font_system, tab_family)
            .unwrap_or(fonts::LineMetrics::DEFAULT);
        let tab_lh = self.tab_metrics.height_at(tab_font_px);
        self.tab_attrs = {
            let db = self.font_system.db();
            fonts::pin_face(db, Attrs::new().family(Family::Name(tab_family)), tab_family)
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
        self.tab_h = (tab_lh + TAB_VPAD * 2.0).round().max(1.0);

        if let Some(gpu) = &mut self.gpu {
            gpu.tab_pool = (0..16)
                .map(|_| {
                    Buffer::new(
                        &mut self.font_system,
                        Metrics::new(tab_font_px, tab_lh),
                    )
                })
                .collect();
        }
        self.sync_term_size();
        log::info!(
            "tab font: {} {:.0}pt; text {:.0}, strip {:.0}",
            tab_family, tab_pt, self.tab_text_h, self.tab_h
        );
    }

    /// Full font re-apply from config (used by config_reload). Resets the live
    /// term size to the config value, then rebuilds both fonts.
    fn apply_fonts(&mut self) {
        let (term_family, term_pt) = config::parse_font(&self.config.term_font);
        self.term_family = Box::leak(term_family.into_boxed_str());
        self.term_pt = term_pt;
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

    fn build_frame(&self, tab: &Tab, show_cursor: bool) -> FrameData {
        let term = tab.term.lock();
        let content = term.renderable_content();
        let palette = Palette::from_colors(content.colors, &self.config.palette);
        let rows = self.rows;
        let cols = self.cols;

        let mut runs: Vec<Run> = Vec::new();
        let mut cur: Option<Run> = None;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 as usize;
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

            let fg = palette.resolve_with(&cell.fg, self.fg, self.bg);
            let bg = palette.resolve_with(&cell.bg, self.fg, self.bg);
            let attrs = self.cell_attrs(cell.c);

            // Continue the current run in place. Building a `String` per cell is
            // ~80k allocations a frame at 4K, and a debug build feels every one.
            if let Some(r) = cur.as_mut() {
                if r.row == row as u32
                    && r.col + r.width == col as u32
                    && r.fg == fg
                    && r.bg == bg
                    && std::ptr::eq(r.attrs, attrs)
                {
                    r.width += 1;
                    r.text.push(cell.c);
                    if let Some(zw) = cell.zerowidth() {
                        r.text.extend(zw.iter().copied());
                    }
                    continue;
                }
            }
            if let Some(r) = cur.take() {
                push_run(&mut runs, r, self.fg, self.bg);
            }
            let mut glyph = String::with_capacity(4);
            glyph.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                glyph.extend(zw.iter());
            }
            cur = Some(Run {
                row: row as u32,
                col: col as u32,
                width: 1,
                fg,
                bg,
                text: glyph,
                attrs,
            });
        }
        if let Some(r) = cur.take() {
            push_run(&mut runs, r, self.fg, self.bg);
        }

        let cursor = if !show_cursor {
            None
        } else {
            match content.cursor.shape {
                alacritty_terminal::vte::ansi::CursorShape::Hidden => None,
                _ => Some((
                    content.cursor.point.column.0 as u32,
                    content.cursor.point.line.0 as u32,
                )),
            }
        };

        FrameData { runs, cursor }
    }

    /// Render the standalone tab overview. Builds a frame per tab (before the
    /// mutable gpu borrow), then hands off to `draw_overview`.
    fn draw(&mut self, window: &Window) {
        let t_start = Instant::now();
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        // The active tab is "focused" iff the window has input focus
        // (covers tab switching, which produces no Focused event).
        {
            let active = &self.tabs[self.active];
            active.term.lock().is_focused = window.has_focus();
        }
        let blink = self.cursor_blink.load(Ordering::SeqCst);
        // The overview is an overlay on the live terminal, so the terminal is
        // still painted underneath it -- that is what the backdrop dims.
        // Every tab's frame is built first (thumbnails are live), before the
        // borrow of `tab` ends.
        let ov_frames: Vec<FrameData> = if self.overview.is_some() {
            self.tabs.iter().map(|t| self.build_frame(t, true)).collect()
        } else {
            Vec::new()
        };
        let frame = self.build_frame(tab, blink);
        let t_built = t_start.elapsed();
        let show_strip = self.show_tab_strip();
        let (w, h) = match &self.gpu {
            Some(g) => (g.config.width, g.config.height),
            None => return,
        };
        if w == 0 || h == 0 {
            return;
        }
        // Moved out of `self` so the drawing code below can keep borrowing
        // other `self` fields freely. Put back before every exit.
        let mut gpu = self.gpu.take().expect("gpu present, checked above");
        let wf = w as f32;
        let hf = h as f32;

        let strip_h = if show_strip { self.tab_h } else { 0.0 };

        gpu.rects.clear();
        // Convert pixel coords to clip space. Clip is -1..1 on both axes with
        // +y pointing up, screen pixels have y pointing down, so flip y.
        let push_rect =
            |v: &mut Vec<gpu::RectVertex>, x: f32, y: f32, rw: f32, rh: f32, c: [f32; 4]| {
                v.extend_from_slice(&gpu::rect_vertices(x, y, rw, rh, c, 0.0, wf, hf));
            };

        // Window background
        // The terminal background is the only translucent layer, matching the
        // original: it sets VTE's background to alpha = 1 - term_opacity and
        // leaves the tab strip opaque. `term_opacity` is therefore an amount of
        // *transparency*, not of opacity -- 0.1 means 10% see-through.
        let term_opacity = effective_opacity(self.config.term_opacity);
        let bg_a = if gpu.alpha_ok && term_opacity > 0.0 {
            1.0 - term_opacity
        } else {
            1.0
        };
        static BG_LOG: std::sync::Once = std::sync::Once::new();
        BG_LOG.call_once(|| {
            log::info!(
                "background alpha {:.3} (alpha_ok {}, term_opacity {:.2} -> effective {:.2}, surface alpha {:?})",
                bg_a,
                gpu.alpha_ok,
                self.config.term_opacity,
                term_opacity,
                gpu.config.alpha_mode
            );
        });
        push_rect(&mut gpu.rects, 0.0, 0.0, w as f32, h as f32, [self.bg[0], self.bg[1], self.bg[2], bg_a]);

        // --- Text areas (tab labels + terminal runs), one prepare() per frame ---
        let mut areas: Vec<TextArea> = Vec::new();
        let bounds = TextBounds { left: 0, top: 0, right: w as i32, bottom: h as i32 };

        // --- Tab strip ---
        let mut tab_hits: Vec<(f32, f32)> = Vec::new();
        let mut add_hit = (0.0f32, 0.0f32);
        if show_strip {
            let bar = self.config.tablist_background.to_f32();
            push_rect(&mut gpu.rects, 0.0, 0.0, wf, self.tab_h, [bar[0], bar[1], bar[2], 1.0]);
            let text_y = ((self.tab_h - self.tab_text_h) / 2.0).round();

            // Measure the labels first. With enough tabs the strip has to scroll
            // to keep the active one visible, which needs the total width up
            // front -- the old code simply stopped drawing past the last pool
            // slot, so the tail of the tab list silently vanished.
            struct Face {
                text: String,
                marker: bool,
                w: f32,
            }
            let faces: Vec<Face> = self
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let text = tab_label(
                        t,
                        i,
                        i == self.active,
                        self.config.tab_title_max_length,
                        self.config.tab_title_short_current,
                        &self.config.tab_shorten_str,
                    );
                    let marker_w = if t.entangled { self.tab_char_w } else { 0.0 };
                    let w = text.chars().count() as f32 * self.tab_char_w
                        + marker_w
                        + TAB_PAD * 2.0;
                    Face { text, marker: t.entangled, w }
                })
                .collect();

            let clock_label: Option<String> = match self.config.tab_prepend.as_deref() {
                Some("clock") => {
                    let (hh, mm) = local_hhmm();
                    Some(format!("{hh:02}:{mm:02} "))
                }
                _ => None,
            };
            let clock_w = clock_label
                .as_ref()
                .map(|s| s.chars().count() as f32 * self.tab_char_w + 2.0)
                .unwrap_or(0.0);
            // "[+]" is three glyphs; the original's add button carries only a
            // couple of pixels of padding, not the tabs' 8px.
            let add_w = 3.0 * self.tab_char_w + 2.0;
            let total =
                clock_w + faces.iter().map(|f| f.w + TAB_GAP).sum::<f32>() + add_w;

            // Scroll only as far as revealing the active tab requires.
            let mut scroll = 0.0f32;
            if total > wf && !faces.is_empty() {
                let active_end = clock_w
                    + add_w
                    + faces.iter().take(self.active + 1).map(|f| f.w + TAB_GAP).sum::<f32>();
                let want = active_end - (wf - add_w - TAB_PAD);
                scroll = want.max(0.0).min((total - wf).max(0.0));
            }

            {
                let fs = &mut self.font_system;
                let attrs = &self.tab_attrs;
                let tpool = &mut gpu.tab_pool;
                let mut slot = 0usize;
                let mut x = -scroll;

                if let Some(label) = clock_label.as_deref() {
                    if slot < tpool.len() {
                        push_tab_label(tpool, slot, fs, attrs, label, x + 1.0, text_y, CLOCK_COLOR, &mut areas, self.tab_char_w, bounds);
                        slot += 1;
                    }
                    x += clock_w;
                }

                // The add button comes FIRST, right after the prepend: the
                // original packs it with `pack_start`, so it renders to the left
                // of every tab. Appending it after the tabs (as this did) was the
                // single most visible difference between the two title bars.
                {
                    let c = self.config.add_button_color.to_f32();
                    let bbg = if self.hover_add { brighten(bar, 1.4) } else { brighten(bar, 1.1) };
                    push_rect(&mut gpu.rects, x, 0.0, add_w, self.tab_h - 1.0, [bbg[0], bbg[1], bbg[2], 1.0]);
                    if slot < tpool.len() {
                        push_tab_label(tpool, slot, fs, attrs, "[+]", x + 1.0, text_y, c, &mut areas, self.tab_char_w, bounds);
                        slot += 1;
                    }
                    add_hit = (x, x + add_w);
                    x += add_w;
                }

                for (i, face) in faces.iter().enumerate() {
                    // One pool slot for the label, one more for the entangle
                    // marker (two areas sharing a Buffer would both render the
                    // buffer's current contents).
                    let need = if face.marker { 2 } else { 1 };
                    if slot + need > tpool.len() {
                        break;
                    }
                    let selected = i == self.active;
                    let t = &self.tabs[i];
                    let mut fgc = if selected {
                        self.config.tab_color_fg_selected.to_f32()
                    } else {
                        self.config.tab_color_fg.to_f32()
                    };
                    if t.group < 8 {
                        fgc = self.config.col[t.group].to_f32();
                    }
                    let bgc = if selected {
                        self.config.tab_color_bg_selected.to_f32()
                    } else {
                        self.config.tab_color_bg.to_f32()
                    };
                    // Leave the bottom pixel to the bar seam so tabs sit *on*
                    // the strip instead of covering it.
                    let body_h = self.tab_h - 1.0;
                    push_rect(&mut gpu.rects, x, 0.0, face.w, body_h, [bgc[0], bgc[1], bgc[2], 1.0]);
                    if self.hover_tab == i && !selected {
                        // Wash of the label's own colour rather than an invented
                        // grey, so hover matches whatever theme is configured.
                        push_rect(&mut gpu.rects, x, 0.0, face.w, body_h, [fgc[0], fgc[1], fgc[2], 0.12]);
                    }
                    let marker_slot = slot;
                    let label_slot = slot + if face.marker { 1 } else { 0 };
                    let mut tx = x + TAB_PAD;
                    if face.marker {
                        push_tab_label(tpool, marker_slot, fs, attrs, "*", tx, text_y, self.config.tab_color_entangle.to_f32(), &mut areas, self.tab_char_w, bounds);
                        tx += self.tab_char_w;
                    }
                    push_tab_label(tpool, label_slot, fs, attrs, &face.text, tx, text_y, fgc, &mut areas, self.tab_char_w, bounds);
                    tab_hits.push((x, x + face.w));
                    slot += need;
                    x += face.w + TAB_GAP;
                }
            }
        }
        self.tab_hits = tab_hits;
        self.add_hit = add_hit;

        // --- Terminal: cell backgrounds ---
        for run in &frame.runs {
            let same_bg = (run.bg[0] - self.bg[0]).abs() < 0.004
                && (run.bg[1] - self.bg[1]).abs() < 0.004
                && (run.bg[2] - self.bg[2]).abs() < 0.004;
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
        // Cursor (hidden on row 0 while renaming; the prompt owns that line)
        if let Some((cx, cy)) = frame.cursor {
            if !(self.renaming && cy == 0) {
                push_rect(
                    &mut gpu.rects,
                    cx as f32 * self.cell_w,
                    strip_h + cy as f32 * self.cell_h,
                    self.cell_w,
                    self.cell_h,
                    [CURSOR_COLOR[0], CURSOR_COLOR[1], CURSOR_COLOR[2], 0.35],
                );
            }
        }

        // --- Terminal text ---
        // SAFETY: pool indices are disjoint (run index < MAX_RUNS), the pool
        // never reallocates after init, and the derived shared references are
        // only used this frame, before any further pool writes.
        {
            let cache = shaping_cache_enabled();
            let fs = &mut self.font_system;
            let pool = &mut gpu.pool;
            for (i, run) in frame.runs.iter().take(MAX_RUNS).enumerate() {
                // Behind the overview the terminal is painted as a colour field
                // only. Text is drawn in a single pass after every rect, so
                // terminal text would otherwise bleed through the tiles; real
                // see-through needs the overlay in its own pass (see TODO).
                if self.overview.is_some() {
                    break;
                }
                // Row 0 is replaced by the rename prompt.
                if self.renaming && run.row == 0 {
                    continue;
                }
                let blank = run.text.chars().all(|c| c == ' ');
                let default_fg =
                    (run.fg[0] - self.fg[0]).abs() + (run.fg[1] - self.fg[1]).abs()
                        + (run.fg[2] - self.fg[2]).abs()
                        < 0.01;
                if blank && default_fg {
                    continue;
                }
                let sig = run_sig(run, self.cell_w);
                if !(cache && gpu.slot_sig.get(i) == Some(&sig)) {
                    gpu.slot_sig[i] = sig;
                    let b = &mut pool[i];
                    b.set_size(Some(run.width as f32 * self.cell_w), None);
                    b.set_text(&run.text, run.attrs, Shaping::Basic, None);
                    b.set_monospace_width(Some(self.cell_w));
                    b.shape_until_scroll(fs, false);
                }
                let buf: &Buffer = unsafe { &*(&pool[i] as *const Buffer) };
                areas.push(TextArea {
                    buffer: buf,
                    left: (run.col as f32 * self.cell_w).round(),
                    top: (strip_h + run.row as f32 * self.cell_h).round(),
                    scale: 1.0,
                    bounds,
                    default_color: glyphon::Color::rgb(
                        (run.fg[0] * 255.0) as u8,
                        (run.fg[1] * 255.0) as u8,
                        (run.fg[2] * 255.0) as u8,
                    ),
                    custom_glyphs: &[],
                });
            }
        }

        // --- Inline rename prompt (overlays terminal row 0) ---
        if self.renaming {
            push_rect(
                &mut gpu.rects,
                0.0,
                strip_h,
                w as f32,
                self.cell_h,
                [self.bg[0], self.bg[1], self.bg[2], 1.0],
            );
            let prompt = format!("Rename: {}", self.rename_draft);
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
                    top: strip_h,
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
        let mut jobs: Vec<gpu::PreviewJob> = Vec::new();
        if let Some(ov) = &self.overview {
            let n = self.tabs.len();
            let sel = ov.selected.min(n.saturating_sub(1));
            gpu.ensure_previews(n, (w, h), &mut self.font_system);
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
                if let Some(b) = gpu.prev_bufs.get_mut(i) {
                    b.set_metrics(glyphon::Metrics::new(self.cell_w, self.cell_h));
                    b.set_size(Some(self.cols as f32 * self.cell_w), None);
                    b.set_monospace_width(Some(self.cell_w));
                    b.set_rich_text(
                        preview_spans(fd, self.rows, &self.mono_attrs)
                            .iter()
                            .map(|(s, a)| (s.as_str(), a.clone())),
                        &self.mono_attrs,
                        Shaping::Basic,
                        None,
                    );
                    b.shape_until_scroll(&mut self.font_system, false);
                }
                gpu.set_prev_sig(i, sig);
                gpu.prev_last[i] = now;
                jobs.push(gpu::PreviewJob {
                    idx: i,
                    top: strip_h,
                    bounds,
                    default_fg: self.fg,
                    rects: preview_rects(
                        fd,
                        &self.bg,
                        &self.config.tablist_background.to_f32(),
                        &self.fg,
                        self.cell_w,
                        self.cell_h,
                        strip_h,
                        self.rows,
                        w,
                        h,
                    ),
                });
                built += 1;
            }
        }
        if let Some(ov) = &self.overview {
            draw_overview(
                &mut gpu,
                &mut self.font_system,
                &self.tabs,
                ov.mode,
                ov.selected,
                &self.config,
                &self.tab_attrs,
                self.tab_text_h,
                self.tab_char_w,
                w,
                h,
                &mut areas,
            );
            // Thumbnails reuse pooled buffers at a different scale, so every
            // cached signature is stale once the overview has drawn.
            gpu.slot_sig.fill(0);
        }
        gpu.render_frame(&mut self.font_system, areas, jobs, window);
        self.gpu = Some(gpu);

        if std::env::var("SPIKE_TRACE").is_ok() {
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
                self.cols,
                self.rows,
                frame.runs.len(),
            );
        }
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    /// Test hook: drain key specs from the $SPIKE_KEYS reader thread and feed
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
                let state: Vec<String> = self
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let title = t.display_title().chars().take(30).collect::<String>();
                        format!("{i}[:{}](g{}e{}){}", t.id, t.group, if t.entangled { 1 } else { 0 }, title)
                    })
                    .collect();
                let cursor = match self.tabs.get(self.active) {
                    Some(t) => {
                        let c = t.term.lock().renderable_content().cursor;
                        let focus = t.term.lock().is_focused;
                        format!("shape={:?} point=({},{}) focus={}", c.shape, c.point.column.0, c.point.line.0, focus)
                    },
                    None => "-".to_string(),
                };
                let surf = match self.gpu.as_ref() {
                    Some(g) => format!("{}x{}", g.config.width, g.config.height),
                    None => "-".to_string(),
                };
                let ren = if self.renaming {
                    format!("renaming={:?}", self.rename_draft)
                } else {
                    String::new()
                };
                log::info!("SPY active={} surf={} cursor=[{}] tabs=[{}] {}", self.active, surf, cursor, state.join(","), ren);
                continue;
            }
            if line == "switcher" {
                // Test hook: open the Alt-held switcher directly (bypass the
                // hold delay, which needs real ModifiersChanged events).
                if self.overview.map(|o| o.mode) != Some(OverviewMode::Switcher)
                    && !self.tabs.is_empty()
                {
                    let cols = grid_cols(self.tabs.len());
                    self.overview = Some(OverviewState {
                        mode: OverviewMode::Switcher,
                        selected: self.active,
                        desired_column: self.active % cols,
                    });
                    self.switcher_open.store(true, Ordering::SeqCst);
                }
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
            if let Some((mods, key)) = parse_key_spec(line) {
                self.modifiers = mods;
                self.handle_key(&key, event_loop);
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn handle_key(&mut self, key: &Key, event_loop: &ActiveEventLoop) {
        // 0. Rename mode eats keys (Enter commits, Esc cancels, Backspace and
        //    characters edit the draft). Nothing reaches the terminal.
        if self.renaming {
            self.handle_rename_key(key);
            return;
        }
        // 1. Overview/switcher is modal: keys navigate/select tabs. Only keys
        //    the overview doesn't claim fall through to the terminal.
        if let Some(ov) = &self.overview {
            match ov.mode {
                OverviewMode::Switcher => self.handle_switcher_key(key),
                OverviewMode::Standalone => self.handle_overview_key(key, event_loop),
            }
            return;
        }
        // 2. Configured bindings
        if let Some(action) = config::find_action(&self.config.bindings, key, self.modifiers, false) {
            self.do_action(&action, event_loop);
            return;
        }
        // 3. Everything else goes to the active terminal
        if let Some(bytes) = key_bytes(key, self.modifiers) {
            // Typing counts as activity: keep the cursor solid (no blink)
            // right after a keypress.
            *self.last_activity.lock().unwrap() = std::time::Instant::now();
            if let Some(tab) = self.tabs.get(self.active) {
                // Entanglement: type into all entangled tabs
                if self.tabs[self.active].entangled {
                    for t in &self.tabs {
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

    fn handle_rename_key(&mut self, key: &Key) {
        use winit::keyboard::NamedKey;
        match key {
            Key::Named(NamedKey::Enter) => {
                let name = self.rename_draft.clone();
                self.renaming = false;
                log::info!("rename_tab -> {:?}", name);
                if let Some(t) = self.tabs.get_mut(self.active) {
                    t.name = if name.is_empty() { None } else { Some(name) };
                }
            },
            Key::Named(NamedKey::Escape) => {
                self.renaming = false;
            },
            Key::Named(NamedKey::Backspace) => {
                self.rename_draft.pop();
            },
            Key::Character(c) => {
                self.rename_draft.push_str(c);
            },
            _ => {},
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Keyboard handling while the tab overview is open (standalone mode).
    /// Mirrors the original TabOverview::handle_key: nav/select/move plus
    /// close_tab, new_tab, and bare number keys for quick switching.
    fn handle_overview_key(&mut self, key: &Key, event_loop: &ActiveEventLoop) {
        let n = self.tabs.len();
        if n == 0 {
            self.overview = None;
            return;
        }
        let cols = grid_cols(n);
        let action = config::find_action(&self.config.bindings, key, self.modifiers, true);
        match action.as_deref() {
            Some("overview_exit") => {
                self.overview = None;
            },
            Some("overview_select") => {
                let sel = self.overview.as_ref().unwrap().selected.min(n - 1);
                self.active = sel;
                self.overview = None;
            },
            Some("overview_new_tab") | Some("new_tab") => {
                self.add_tab();
                let newn = self.tabs.len();
                let mode = self.overview.map(|o| o.mode).unwrap_or(OverviewMode::Standalone);
                let ov = self.overview.get_or_insert(OverviewState {
                    selected: 0,
                    desired_column: 0,
                    mode,
                });
                ov.selected = newn - 1;
                ov.desired_column = (newn - 1) % grid_cols(newn);
            },
            Some("close_tab") => {
                let sel = self.overview.as_ref().unwrap().selected.min(n - 1);
                self.close_tab_at(sel, event_loop);
                if self.tabs.is_empty() {
                    self.overview = None;
                } else {
                    let newn = self.tabs.len();
                    let ov = self.overview.as_mut().unwrap();
                    if ov.selected >= newn {
                        ov.selected = newn - 1;
                    }
                    ov.desired_column = ov.selected % grid_cols(newn);
                }
            },
            Some("overview_nav_left") => {
                let ov = self.overview.as_mut().unwrap();
                if ov.selected > 0 {
                    ov.selected -= 1;
                    ov.desired_column = ov.selected % cols;
                }
            },
            Some("overview_nav_right") => {
                let ov = self.overview.as_mut().unwrap();
                if ov.selected < n - 1 {
                    ov.selected += 1;
                    ov.desired_column = ov.selected % cols;
                }
            },
            Some("overview_nav_up") => {
                let ov = self.overview.as_mut().unwrap();
                let row = ov.selected / cols;
                if row > 0 {
                    ov.selected = ((row - 1) * cols + ov.desired_column).min(n - 1);
                }
            },
            Some("overview_nav_down") => {
                let ov = self.overview.as_mut().unwrap();
                let row = ov.selected / cols;
                let target = (row + 1) * cols + ov.desired_column;
                if target < n && target / cols > row {
                    ov.selected = target;
                }
            },
            Some("overview_first") => {
                let ov = self.overview.as_mut().unwrap();
                ov.selected = 0;
                ov.desired_column = 0;
            },
            Some("overview_last") => {
                let ov = self.overview.as_mut().unwrap();
                ov.selected = n - 1;
                ov.desired_column = (n - 1) % cols;
            },
            Some("overview_move_left") => {
                let sel = self.overview.as_ref().unwrap().selected;
                if sel > 0 {
                    self.tabs.swap(sel, sel - 1);
                    let ov = self.overview.as_mut().unwrap();
                    ov.selected = sel - 1;
                    ov.desired_column = (sel - 1) % cols;
                }
            },
            Some("overview_move_right") => {
                let sel = self.overview.as_ref().unwrap().selected;
                if sel < n - 1 {
                    self.tabs.swap(sel, sel + 1);
                    let ov = self.overview.as_mut().unwrap();
                    ov.selected = sel + 1;
                    ov.desired_column = (sel + 1) % cols;
                }
            },
            Some("overview_move_up") => {
                let sel = self.overview.as_ref().unwrap().selected;
                if sel >= cols {
                    let t = self.tabs.remove(sel);
                    let newpos = sel - cols;
                    self.tabs.insert(newpos, t);
                    let ov = self.overview.as_mut().unwrap();
                    ov.selected = newpos;
                    ov.desired_column = newpos % grid_cols(self.tabs.len());
                }
            },
            Some("overview_move_down") => {
                let sel = self.overview.as_ref().unwrap().selected;
                if sel + cols < n {
                    let t = self.tabs.remove(sel);
                    let newpos = sel + cols;
                    self.tabs.insert(newpos, t);
                    let ov = self.overview.as_mut().unwrap();
                    ov.selected = newpos;
                    ov.desired_column = newpos % grid_cols(self.tabs.len());
                }
            },
            _ => {
                // Bare number keys for quick switching (standalone: no mods),
                // matching the original. Alt+digit (goto_tab_*) has a modifier
                // and is intentionally ignored while the overview is open.
                if self.modifiers.is_empty() {
                    if let Key::Character(c) = key {
                        if let Some(ch) = c.chars().next() {
                            if let Some(d) = ch.to_digit(10) {
                                let idx = if d == 0 { 9 } else { (d - 1) as usize };
                                if idx < n {
                                    self.active = idx;
                                    self.overview = None;
                                }
                            }
                        }
                    }
                }
            },
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Dismiss the Alt-held switcher (only if that variant is open).
    fn close_switcher(&mut self) {
        if self.overview.as_ref().map(|o| o.mode) == Some(OverviewMode::Switcher) {
            self.overview = None;
            self.switcher_open.store(false, Ordering::SeqCst);
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Keys while the Alt-held switcher is open. Alt is held during navigation,
    /// so match on the logical key (arrows, Home/End, digits) regardless of the
    /// Alt modifier. Navigation changes the active tab live, so the preview and
    /// the eventual commit are the same tab; Alt release just dismisses the
    /// popup. Unhandled keys are dropped (the switcher is modal while held).
    fn handle_switcher_key(&mut self, key: &Key) {
        use winit::keyboard::NamedKey;
        let n = self.tabs.len();
        if n == 0 {
            self.close_switcher();
            return;
        }
        let cols = grid_cols(n);
        let handled = match key {
            Key::Named(NamedKey::ArrowLeft) => {
                self.active = (self.active + n - 1) % n;
                true
            },
            Key::Named(NamedKey::ArrowRight) => {
                self.active = (self.active + 1) % n;
                true
            },
            Key::Named(NamedKey::ArrowUp) => {
                let r = self.active / cols;
                if r > 0 {
                    let dc = self.active % cols;
                    self.active = ((r - 1) * cols + dc).min(n - 1);
                }
                true
            },
            Key::Named(NamedKey::ArrowDown) => {
                let r = self.active / cols;
                let t = (r + 1) * cols + (self.active % cols);
                if t < n && t / cols > r {
                    self.active = t;
                }
                true
            },
            Key::Named(NamedKey::Home) => {
                self.active = 0;
                true
            },
            Key::Named(NamedKey::End) => {
                self.active = n - 1;
                true
            },
            Key::Named(NamedKey::Escape) => {
                self.close_switcher();
                true
            },
            Key::Character(c) => {
                // Alt+digit: jump to that tab (Alt is held in the switcher).
                match c.chars().next().and_then(|ch| ch.to_digit(10)) {
                    Some(d) => {
                        let idx = if d == 0 { 9 } else { (d - 1) as usize };
                        if idx < n {
                            self.active = idx;
                        }
                        true
                    },
                    None => false,
                }
            },
            _ => false,
        };
        // Keep the highlighted tile in sync with the active tab.
        if let Some(ov) = self.overview.as_mut() {
            ov.selected = self.active;
        }
        if handled {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }

    fn do_action(&mut self, action: &str, event_loop: &ActiveEventLoop) {
        let n = self.tabs.len();
        match action {
            "new_tab" => self.add_tab(),
            "close_tab" => self.close_tab_at(self.active, event_loop),
            "prev_tab" | "prev_tab_row" => {
                if n > 1 {
                    self.active = (self.active + n - 1) % n;
                }
            },
            "next_tab" | "next_tab_row" => {
                if n > 1 {
                    self.active = (self.active + 1) % n;
                }
            },
            "first_tab" => self.active = 0,
            "last_tab" => self.active = n.saturating_sub(1),
            "show_tab_overview" => {
                // Toggle: close if open, else open (standalone mode).
                if self.overview.is_some() {
                    self.overview = None;
                } else if !self.tabs.is_empty() {
                    let cols = grid_cols(self.tabs.len());
                    self.overview = Some(OverviewState {
                        mode: OverviewMode::Standalone,
                        selected: self.active,
                        desired_column: self.active % cols,
                    });
                }
            },
            "move_prev_tab" => {
                if n > 1 && self.active > 0 {
                    self.tabs.swap(self.active, self.active - 1);
                    self.active -= 1;
                }
            },
            "move_next_tab" => {
                if n > 1 && self.active < n - 1 {
                    self.tabs.swap(self.active, self.active + 1);
                    self.active += 1;
                }
            },
            "move_first_tab" => {
                if n > 1 && self.active > 0 {
                    let t = self.tabs.remove(self.active);
                    self.tabs.insert(0, t);
                    self.active = 0;
                }
            },
            "move_last_tab" => {
                if n > 1 && self.active < n - 1 {
                    let t = self.tabs.remove(self.active);
                    self.tabs.push(t);
                    self.active = n - 1;
                }
            },
            "toggle_entangle" => {
                if let Some(t) = self.tabs.get_mut(self.active) {
                    t.entangled = !t.entangled;
                }
            },
            "next_color" => {
                if let Some(t) = self.tabs.get_mut(self.active) {
                    t.group = if t.group >= 8 { 0 } else { t.group + 1 };
                }
            },
            "prev_color" => {
                if let Some(t) = self.tabs.get_mut(self.active) {
                    t.group = if t.group <= 0 || t.group >= 8 { 7 } else { t.group - 1 };
                }
            },
            "toggle_hide_tabs" => {
                self.tablist_hidden = !self.tablist_hidden;
                self.sync_term_size();
            },
            "config_reload" => {
                let old_term = self.config.term_font.clone();
                let old_tab = self.config.tablist_font.clone();
                self.config = Config::load();
                let font_changed =
                    old_term != self.config.term_font || old_tab != self.config.tablist_font;
                self.apply_config(font_changed);
                log::info!("config reloaded");
            },
            "goto_tab_0" | "goto_tab_1" | "goto_tab_2" | "goto_tab_3" | "goto_tab_4"
            | "goto_tab_5" | "goto_tab_6" | "goto_tab_7" | "goto_tab_8" | "goto_tab_9" => {
                let i = action.chars().last().unwrap().to_digit(10).unwrap() as usize;
                let target = if i == 0 { n.saturating_sub(1) } else { i - 1 };
                if target < n {
                    self.active = target;
                }
            },
            "enlarge_font" => self.zoom_term(true),
            "shrink_font" => self.zoom_term(false),
            "rename_tab" => {
                // Pre-fill the draft with the current displayed title; the
                // inline prompt takes over from the next key.
                self.rename_draft = self.tabs[self.active].display_title();
                self.renaming = true;
            },
            // Not yet ported
            "show_search" | "show_search_all" | "search_prev" | "search_next" => {
                log::debug!("unported action: {action}");
            },
            _ => {},
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// Build the tab strip label text, mimicking the Python tab_format.
/// Scale a colour toward white, clamped. Mirrors the original's `brighten()`,
/// which GTK uses for the add-tab button background.
fn brighten(c: [f32; 3], f: f32) -> [f32; 3] {
    [
        (c[0] * f).min(1.0),
        (c[1] * f).min(1.0),
        (c[2] * f).min(1.0),
    ]
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

fn tab_label(
    t: &Tab,
    idx: usize,
    selected: bool,
    max_len: usize,
    cur_len: usize,
    shorten: &str,
) -> String {
    let mut name = t.display_title();
    let limit = if selected { cur_len } else { max_len };
    if limit > 0 && name.chars().count() > limit {
        name = name.chars().take(limit).collect::<String>() + shorten;
    }
    let marker = if t.entangled { "*" } else { "" };
    let _ = idx;
    format!("[{marker}{name}]")
}
/// Shape a short tab-strip label into a TextArea using the tab pool.
/// SAFETY: pool indices are unique per call (caller passes distinct slots);
/// the pool never reallocates after init; the derived shared reference is
/// only used this frame.
fn push_tab_label(
    tpool: &mut [Buffer],
    slot: usize,
    fs: &mut FontSystem,
    attrs: &Attrs,
    text: &str,
    x: f32,
    y: f32,
    color: [f32; 3],
    areas: &mut Vec<TextArea>,
    char_w: f32,
    bounds: TextBounds,
) {
    let b = &mut tpool[slot];
    b.set_size(None, None);
    b.set_text(text, attrs, Shaping::Basic, None);
    b.set_monospace_width(Some(char_w));
    b.shape_until_scroll(fs, false);
    let buf: &Buffer = unsafe { &*(b as *const Buffer) };
    areas.push(TextArea {
        buffer: buf,
        left: x,
        top: y,
        scale: 1.0,
        bounds,
        default_color: glyphon::Color::rgb(
            (color[0] * 255.0) as u8,
            (color[1] * 255.0) as u8,
            (color[2] * 255.0) as u8,
        ),
        custom_glyphs: &[],
    });
}

/// Push one text area into an overview tile, with a `scale` (for scaled
/// thumbnails) and an optional monospace width. Mirrors `push_tab_label` but
/// generalised. `slot` must be < pool.len().
fn push_ov_text(
    pool: &mut [Buffer],
    slot: usize,
    fs: &mut FontSystem,
    attrs: &Attrs,
    text: &str,
    x: f32,
    y: f32,
    scale: f32,
    color: [f32; 3],
    monow: Option<f32>,
    size_w: Option<f32>,
    areas: &mut Vec<TextArea>,
    bounds: TextBounds,
) {
    if slot >= pool.len() || text.is_empty() {
        return;
    }
    let b = &mut pool[slot];
    if let Some(mw) = monow {
        b.set_monospace_width(Some(mw));
    }
    b.set_size(size_w, None);
    b.set_text(text, attrs, Shaping::Basic, None);
    b.shape_until_scroll(fs, false);
    let buf: &Buffer = unsafe { &*(b as *const Buffer) };
    areas.push(TextArea {
        buffer: buf,
        left: x,
        top: y,
        scale,
        bounds,
        default_color: glyphon::Color::rgb(
            (color[0] * 255.0) as u8,
            (color[1] * 255.0) as u8,
            (color[2] * 255.0) as u8,
        ),
        custom_glyphs: &[],
    });
}

/// Render the standalone tab overview: a dark backdrop, a two-line banner, and
/// a centred grid of tiles. Each tile shows the tab's group colour, its title,
/// and a scaled thumbnail of the terminal content (built from `frames`).
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
    mix(&mut h, fd.runs.len() as u64);
    for run in &fd.runs {
        mix(&mut h, run.row as u64);
        mix(&mut h, run.col as u64);
        mix(&mut h, run.width as u64);
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

/// One whole screen as spans for a single multi-line `Buffer`: a line per
/// terminal row, gaps padded with spaces so columns land on the grid, and a
/// newline ending every row. An empty row still gets a space, because a line
/// with no glyphs may not advance by a full line height.
fn preview_spans(
    fd: &FrameData,
    rows: usize,
    default: &Attrs<'static>,
) -> Vec<(String, Attrs<'static>)> {
    let mut spans: Vec<(String, Attrs<'static>)> = Vec::with_capacity(fd.runs.len() + rows);
    let mut idx = 0usize;
    for r in 0..rows as u32 {
        let mut col = 0u32;
        let mut any = false;
        while idx < fd.runs.len() && fd.runs[idx].row == r {
            let run = &fd.runs[idx];
            // Runs arrive in row order. Anything out of order (a wild redraw) is
            // skipped rather than double-drawn in the wrong column.
            //
            // Runs also carry the padding spaces out to the end of the line. They
            // are invisible in a thumbnail too, but shaping 545 of them per row --
            // 80k glyphs a refresh at full size -- cost seconds, so they are
            // dropped here along with runs that are nothing but padding. Leading
            // gaps stay: they are what puts text in the right column.
            let body = run.text.trim_end();
            if !body.is_empty() && run.col >= col {
                if run.col > col {
                    spans.push((" ".repeat((run.col - col) as usize), default.clone()));
                }
                spans.push((body.to_string(), (*run.attrs).clone()));
                col = run.col.saturating_add(body.chars().count() as u32);
                any = true;
            }
            idx += 1;
        }
        spans.push((
            if any {
                "\n".to_string()
            } else {
                " \n".to_string()
            },
            default.clone(),
        ));
    }
    spans
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
            [fg[0], fg[1], fg[2], 0.85],
        );
    }
    v
}

fn draw_overview(
    gpu: &mut Gpu,
    fs: &mut FontSystem,
    tabs: &[Tab],
    mode: OverviewMode,
    selected: usize,
    config: &Config,
    tab_attrs: &Attrs,
    tab_text_h: f32,
    tab_char_w: f32,
    w: u32,
    h: u32,
    areas: &mut Vec<TextArea>,
) {
    let wf = w as f32;
    let hf = h as f32;
    let push_rect =
        |v: &mut Vec<gpu::RectVertex>, x: f32, y: f32, rw: f32, rh: f32, c: [f32; 4]| {
            v.extend_from_slice(&gpu::rect_vertices(x, y, rw, rh, c, 0.0, wf, hf));
        };
    let push_rrect =
        |v: &mut Vec<gpu::RectVertex>, x: f32, y: f32, rw: f32, rh: f32, c: [f32; 4], r: f32| {
            v.extend_from_slice(&gpu::rect_vertices(x, y, rw, rh, c, r, wf, hf));
        };

    let bounds = TextBounds { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    let pool = &mut gpu.pool;
    let mut slot = 0usize;

    // Dark backdrop over the whole window.
    // The live terminal is painted underneath now, so this is a dim rather than
    // a cover. The Alt-held switcher is a peek over running output, so it stays
    // lighter than the standalone overview even when the config asks for opaque.
    let dim = match mode {
        OverviewMode::Switcher => config.tab_overview_opacity.min(0.7),
        OverviewMode::Standalone => config.tab_overview_opacity,
    };
    push_rect(&mut gpu.rects, 0.0, 0.0, wf, hf, [0.0, 0.0, 0.0, dim]);

    let n = tabs.len();
    let cols = grid_cols(n);
    let rows = (n + cols - 1) / cols;

    // Banner, using the tab (larger) font. Standalone gets a two-line header
    // with the key hints; the Alt-held switcher gets a single quiet label.

    // Grid geometry.
    let pad = 10.0;
    let spacing = 12.0;
    let banner_h = if mode == OverviewMode::Standalone { 58.0 } else { 40.0 };
    let thumb_w = (config.tab_overview_thumb_width as f32)
        .min((wf - 100.0) / 4.0 - 20.0)
        .max(60.0);
    let thumb_h = (config.tab_overview_thumb_height as f32)
        .min(thumb_w * 0.75)
        .max(40.0);
    let header_h = tab_text_h + 6.0;
    let tile_w = thumb_w + pad * 2.0;
    let tile_h = header_h + thumb_h + pad * 2.0;
    let grid_w = cols as f32 * tile_w + (cols.saturating_sub(1)) as f32 * spacing;
    let grid_h = rows as f32 * tile_h + (rows.saturating_sub(1)) as f32 * spacing;
    let grid_x = ((wf - grid_w) / 2.0).max(8.0);
    let grid_y = (banner_h + ((hf - banner_h - grid_h) / 2.0)).max(banner_h);

    // Headers sit on a dark pill: the live tab strip shows through now, and bare
    // text over it was unreadable.
    let title = if mode == OverviewMode::Standalone { "TAB OVERVIEW MODE" } else { "Tab Switcher" };
    let title_col: [f32; 3] =
        if mode == OverviewMode::Standalone { [1.0, 1.0, 1.0] } else { [0.36, 0.75, 0.75] };
    let (bx, by) = if mode == OverviewMode::Standalone {
        (16.0, 12.0)
    } else {
        // Just above the tiles, which keeps it clear of the tab strip.
        (grid_x, (grid_y - tab_text_h - 14.0).max(4.0))
    };
    push_rrect(
        &mut gpu.rects, bx - 8.0, by - 5.0,
        title.len() as f32 * tab_char_w + 16.0, tab_text_h + 10.0,
        [0.0, 0.0, 0.0, 0.62], 5.0,
    );
    push_ov_text(pool, slot, fs, tab_attrs, title, bx, by, 1.0, title_col, None, None, areas, bounds);
    slot += 1;
    if mode == OverviewMode::Standalone {
        let hint = "ESC exit | Enter select | Alt+Return new | Arrows move | Shift+Arrows reorder | Ctrl+Alt+X close";
        push_rrect(
            &mut gpu.rects, 8.0, 31.0,
            hint.len() as f32 * tab_char_w + 16.0, tab_text_h + 10.0,
            [0.0, 0.0, 0.0, 0.62], 5.0,
        );
        push_ov_text(pool, slot, fs, tab_attrs, hint, 16.0, 36.0, 1.0, [0.72, 0.72, 0.72], None, None, areas, bounds);
        slot += 1;
    }

    let radius = 8.0f32;
    let chip = tab_text_h.max(14.0);
    // Tiles that sample a real frame, collected during the loop and pushed to
    // `gpu` afterwards: the tile text holds `gpu` borrowed as `&mut gpu.pool`.
    let mut tile_previews: Vec<(usize, f32, f32, f32, f32)> = Vec::new();

    for (idx, t) in tabs.iter().enumerate() {
        let row = idx / cols;
        let col = idx % cols;
        let tx = grid_x + col as f32 * (tile_w + spacing);
        let ty = grid_y + row as f32 * (tile_h + spacing);
        let selected = idx == selected;
        let accent = if t.group < 8 {
            config.col[t.group].to_f32()
        } else {
            [0.36, 0.75, 0.75]
        };

        // Depth: three expanding, fading rounded rects behind the tile. A real
        // blur needs a second render pass; stacked rects read the same at this
        // size and cost a handful of triangles.
        for (grow, a) in [(10.0f32, 0.05f32), (6.0, 0.08), (3.0, 0.12)] {
            push_rrect(
                &mut gpu.rects, tx - grow, ty - grow + 4.0,
                tile_w + grow * 2.0, tile_h + grow * 2.0,
                [0.0, 0.0, 0.0, a], radius + grow, 
            );
        }
        // Frame: the selected tile is ringed in its own group colour, the rest
        // get a faint light edge.
        if selected {
            push_rrect(
                &mut gpu.rects, tx - 2.0, ty - 2.0, tile_w + 4.0, tile_h + 4.0,
                [accent[0], accent[1], accent[2], 0.95], radius + 2.0, 
            );
        } else {
            push_rrect(
                &mut gpu.rects, tx - 1.0, ty - 1.0, tile_w + 2.0, tile_h + 2.0,
                [1.0, 1.0, 1.0, 0.12], radius + 1.0, 
            );
        }
        // Tiles are fully opaque on purpose: all text is drawn in a single pass
        // after all rects, so a translucent tile had the live terminal's text
        // bleeding through it. The dim between tiles is where the live terminal
        // shows; inside a tile you see only that tab's preview.
        let bgc = if selected { [0.13, 0.30, 0.30, 1.0] } else { [0.09, 0.09, 0.09, 1.0] };
        push_rrect(&mut gpu.rects, tx, ty, tile_w, tile_h, bgc, radius);

        // Header: index chip in the accent colour, then the title.
        let hx = tx + pad;
        let hy = ty + pad;
        push_rrect(
            &mut gpu.rects, hx, hy, chip, chip,
            [accent[0], accent[1], accent[2], 1.0], 3.0, 
        );
        let digits = if idx < 9 { 1 } else { 2 };
        let num_x = hx + (chip - digits as f32 * tab_char_w) / 2.0;
        push_ov_text(
            pool, slot, fs, tab_attrs, &format!("{}", idx + 1),
            num_x, hy - 1.0, 1.0, [0.05, 0.05, 0.05], None, None, areas, bounds,
        );
        slot += 1;

        let room = ((tile_w - pad * 2.0 - chip - 6.0) / tab_char_w).max(1.0) as usize;
        let title: String = t.display_title().chars().take(room).collect();
        let header_color: [f32; 3] = if selected { [1.0, 1.0, 1.0] } else { [0.78, 0.78, 0.78] };
        push_ov_text(
            pool, slot, fs, tab_attrs, &title,
            hx + chip + 6.0, hy - 1.0, 1.0, header_color, None, None, areas, bounds,
        );
        slot += 1;

        // Preview area: a slightly deeper well inside the tile, for depth.
        let thx = tx + pad;
        let thy = ty + pad + header_h;
        push_rrect(
            &mut gpu.rects, thx - 1.0, thy - 1.0, thumb_w + 2.0, thumb_h + 2.0,
            [0.03, 0.03, 0.03, 1.0], 4.0,
        );
        // Preview: a real render of the tab, sampled down. Every tile has one.
        // A tile whose frame was not rebuilt this tick simply keeps showing its
        // previous frame, so previews run at a few frames a second instead of
        // stalling the overlay (see the rebuild budget in `draw`).
        {
            // Fit the whole window into the thumb, aspect intact, so the tile is
            // a picture of the window -- the tab strip comes with it. The quad is
            // pushed after the loop: `pool` holds `gpu` borrowed until then.
            let s = (thumb_w / wf).min(thumb_h / hf);
            let fw = wf * s;
            let fh = hf * s;
            tile_previews.push((
                idx,
                thx + (thumb_w - fw) / 2.0,
                thy + (thumb_h - fh) / 2.0,
                fw,
                fh,
            ));
        }
        // Entangled marker.
        if t.entangled {
            push_ov_text(
                pool, slot, fs, tab_attrs, "* entangled",
                thx, thy + thumb_h - tab_text_h, 1.0,
                [1.0, 0.35, 0.35], None, None, areas, bounds,
            );
            slot += 1;
        }
    }

    // Deferred until here: `push_preview_quad` needs `gpu`, and the tile text
    // held it borrowed as `&mut gpu.pool` through the loop above.
    for (tab, x, y, fw, fh) in tile_previews {
        gpu.push_preview_quad(tab, x, y, fw, fh, wf, hf);
    }
}

/// Parse a test key spec like "alt+shift+t", "ctrl+c", "esc", "f5".
/// Test hook: read key specs from the $SPIKE_KEYS FIFO in a background
/// thread and forward them over a channel. Returns None when unset.
fn spawn_key_reader(
    window_slot: Arc<Mutex<Option<Arc<Window>>>>,
) -> Option<std::sync::mpsc::Receiver<String>> {
    let path = std::env::var("SPIKE_KEYS").ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        // A FIFO returns EOF when every writer closes. Our test harness opens
        // and closes the FIFO per line, so reopen after each EOF and keep the
        // reader alive (File::open blocks until the next writer attaches).
        loop {
            let Ok(file) = std::fs::File::open(&path) else {
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
                    if let Some(w) = slot.as_ref() {
                        w.request_redraw();
                    }
                }
                // Wake the UI thread so the spec is processed promptly.
                if let Some(window) = window_slot.lock().unwrap().as_ref() {
                    window.request_redraw();
                }
            }
        }
    });
    Some(rx)
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

fn key_bytes(k: &Key, mods: ModifiersState) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let esc_seq = |body: &[u8]| -> Vec<u8> {
        let mut v = vec![0x1b];
        v.extend_from_slice(body);
        v
    };
    match k {
        Key::Character(c) => {
            if ctrl {
                Some(match c.as_str() {
                    " " => vec![0x00],
                    "a" | "A" => vec![0x01],
                    "b" | "B" => vec![0x02],
                    "c" | "C" => vec![0x03],
                    "d" | "D" => vec![0x04],
                    "e" | "E" => vec![0x05],
                    "f" | "F" => vec![0x06],
                    "g" | "G" => vec![0x07],
                    "h" | "H" => vec![0x08],
                    "i" | "I" => vec![0x09],
                    "j" | "J" => vec![0x0a],
                    "k" | "K" => vec![0x0b],
                    "l" | "L" => vec![0x0c],
                    "m" | "M" => vec![0x0d],
                    "n" | "N" => vec![0x0e],
                    "o" | "O" => vec![0x0f],
                    "p" | "P" => vec![0x10],
                    "q" | "Q" => vec![0x11],
                    "r" | "R" => vec![0x12],
                    "s" | "S" => vec![0x13],
                    "t" | "T" => vec![0x14],
                    "u" | "U" => vec![0x15],
                    "v" | "V" => vec![0x16],
                    "w" | "W" => vec![0x17],
                    "x" | "X" => vec![0x18],
                    "y" | "Y" => vec![0x19],
                    "z" | "Z" => vec![0x1a],
                    "[" => vec![0x1b],
                    "\\" => vec![0x1c],
                    "]" => vec![0x1d],
                    "^" => vec![0x1e],
                    "_" => vec![0x1f],
                    _ => return None,
                })
            } else if alt {
                let mut v = vec![0x1b];
                v.extend_from_slice(c.as_bytes());
                Some(v)
            } else {
                Some(c.as_bytes().to_vec())
            }
        },
        Key::Named(n) => Some(match n {
            NamedKey::Space => vec![0x20],
            NamedKey::Enter => vec![0x0d],
            NamedKey::Backspace => vec![0x7f],
            NamedKey::Tab => vec![0x09],
            NamedKey::Escape => vec![0x1b],
            NamedKey::ArrowUp => esc_seq(b"[A"),
            NamedKey::ArrowDown => esc_seq(b"[B"),
            NamedKey::ArrowRight => esc_seq(b"[C"),
            NamedKey::ArrowLeft => esc_seq(b"[D"),
            NamedKey::Home => esc_seq(b"[H"),
            NamedKey::End => esc_seq(b"[F"),
            NamedKey::PageUp => esc_seq(b"[5~"),
            NamedKey::PageDown => esc_seq(b"[6~"),
            NamedKey::Delete => esc_seq(b"[3~"),
            NamedKey::Insert => esc_seq(b"[2~"),
            NamedKey::F1 => esc_seq(b"[OP"),
            NamedKey::F2 => esc_seq(b"[OQ"),
            NamedKey::F3 => esc_seq(b"[OR"),
            NamedKey::F4 => esc_seq(b"[OS"),
            NamedKey::F5 => esc_seq(b"[15~"),
            NamedKey::F6 => esc_seq(b"[17~"),
            NamedKey::F7 => esc_seq(b"[18~"),
            NamedKey::F8 => esc_seq(b"[19~"),
            NamedKey::F9 => esc_seq(b"[20~"),
            NamedKey::F10 => esc_seq(b"[21~"),
            NamedKey::F11 => esc_seq(b"[23~"),
            NamedKey::F12 => esc_seq(b"[24~"),
            _ => return None,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ApplicationHandler
// ---------------------------------------------------------------------------

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
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
                            .with_transparent(self.config.term_opacity > 0.0)
                            .with_title("mypixels"),
                    )
                    .expect("window"),
            );
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
            self.init_gpu(event_loop, &window);
            *self.window_slot.lock().unwrap() = Some(Arc::clone(&window));
            self.window = Some(window);
            self.sync_term_size();
            if self.tabs.is_empty() {
                self.add_tab();
            }
        }
        let _ = event_loop;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_test_keys(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            },
            WindowEvent::RedrawRequested => {
                self.drain_test_keys(event_loop);
                // Open the Alt-held switcher if the clock asked and Alt is
                // still held (guards against the press/release race).
                if self.switcher_show_req.swap(false, Ordering::SeqCst)
                    && self.switcher_alt_at.lock().unwrap().is_some()
                    && !self.tabs.is_empty()
                    && self.overview.map(|o| o.mode) != Some(OverviewMode::Switcher)
                {
                    let cols = grid_cols(self.tabs.len());
                    self.overview = Some(OverviewState {
                        mode: OverviewMode::Switcher,
                        selected: self.active,
                        desired_column: self.active % cols,
                    });
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
                if !to_close.is_empty() {
                    // map stable ids back to current indices (ids are stable)
                    for id in to_close.iter().rev() {
                        if let Some(pos) = self.tabs.iter().position(|t| t.id == *id) {
                            self.close_tab_at(pos, event_loop);
                        }
                    }
                    if self.tabs.is_empty() {
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
                let in_strip = self.show_tab_strip() && py < self.tab_h;
                let mut hover = usize::MAX;
                let mut hover_add = false;
                if in_strip {
                    for (i, (x0, x1)) in self.tab_hits.iter().enumerate() {
                        if px >= *x0 && px < *x1 {
                            hover = i;
                            break;
                        }
                    }
                    let (a0, a1) = self.add_hit;
                    hover_add = px >= a0 && px < a1;
                }
                if hover != self.hover_tab || hover_add != self.hover_add {
                    self.hover_tab = hover;
                    self.hover_add = hover_add;
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if self.hover_tab < self.tabs.len() {
                    if self.hover_tab != self.active {
                        self.active = self.hover_tab;
                        window.request_redraw();
                    }
                } else if self.hover_add {
                    self.add_tab();
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event: key, .. } => {
                if key.state == ElementState::Pressed && !key.repeat {
                    self.handle_key(&key.logical_key, event_loop);
                }
            },
            WindowEvent::ModifiersChanged(m) => {
                let new = m.state();
                let alt_up = new.alt_key() && !self.modifiers.alt_key();
                let alt_down = !new.alt_key() && self.modifiers.alt_key();
                self.modifiers = new;
                if alt_up {
                    // Alt pressed: start the switcher hold timer.
                    *self.switcher_alt_at.lock().unwrap() = Some(std::time::Instant::now());
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
                if let Some(tab) = self.tabs.get(self.active) {
                    tab.term.lock().is_focused = focused;
                }
                window.request_redraw();
            },
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
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
        for tab in self.tabs.iter() {
            let _ = tab.sender.send(Msg::Shutdown);
        }
    }
}

/// TEMPORARY compensation for the difference between our one alpha stage and the
/// original's, applied by default; `SPIKE_OPACITY` replaces it outright.
///
/// Two readings from the user, at the values actually used:
///   term_opacity 0.1 -> ours too dark (needs to be more see-through)
///   term_opacity 0.6 -> ours too transparent (needs to be less see-through)
/// The first wants eff > x, the second eff < x, so the correction must cross and
/// no constant or scale factor can do it. Modelled as a bump that ramps in over
/// the first tenth and a flatter slope above it:
///   eff = 0.75*x + 0.105*min(1, x/0.1)
/// which gives 0.18 at 0.1 and 0.555 at 0.6, is continuous, monotone, and 0 at 0
/// so an opaque config stays opaque. That is curve-fitting, not a derivation --
/// the likely real cause is that VTE composites its *text* inside the translucent
/// layer as well, which lifts the whole appearance rather than the background.
/// Re-check after the window/tint layer is modelled; then keep or delete this.
fn effective_opacity(cfg: f32) -> f32 {
    static OVERRIDE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    let over = *OVERRIDE.get_or_init(|| {
        std::env::var("SPIKE_OPACITY")
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

fn main() {
    env_logger::init();
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
            attrs: Box::leak(Box::new(Attrs::new())),
        }
    }

    fn joined(spans: &[(String, Attrs<'static>)]) -> String {
        spans.iter().map(|(s, _)| s.as_str()).collect()
    }

    #[test]
    fn one_line_per_row_with_gaps_holding_the_columns() {
        let fd = FrameData {
            runs: vec![run(0, 0, "ab   "), run(2, 4, "cd")],
            cursor: None,
        };
        // Trailing padding is invisible in a thumbnail and expensive to shape, so
        // it is gone; leading spaces stay because they place the text.
        assert_eq!(joined(&preview_spans(&fd, 4, &Attrs::new())), "ab\n \n    cd\n \n");
    }

    #[test]
    fn an_empty_row_still_advances_a_line() {
        let fd = FrameData {
            runs: vec![],
            cursor: None,
        };
        assert_eq!(joined(&preview_spans(&fd, 3, &Attrs::new())), " \n \n \n");
    }

    #[test]
    fn signatures_see_text_and_cursor_but_nothing_else() {
        let mut fd = FrameData {
            runs: vec![run(0, 0, "x")],
            cursor: None,
        };
        let base = frame_sig(&fd, 7.0);
        fd.cursor = Some((1, 0));
        assert_ne!(base, frame_sig(&fd, 7.0), "the cursor is part of the frame");
        fd.cursor = None;
        fd.runs[0].text = "y".into();
        assert_ne!(base, frame_sig(&fd, 7.0), "so is the text");
        let again = frame_sig(&fd, 7.0);
        assert_eq!(again, frame_sig(&fd, 7.0), "same input, same signature");
    }
}
