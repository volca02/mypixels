//! mypixels (Rust port): tabbed terminal.
//! alacritty_terminal (emulation + pty) rendered with glyphon (cosmic_text)
//! on wgpu in a winit window.
//!
//! Usage: mypixels-rs [command [args...]]   (defaults to $SHELL or bash)

mod config;
mod gpu;
mod tab;

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
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use config::Config;
use gpu::Gpu;
use tab::{SpikeListener, Tab};

const HISTORY: usize = 10_000;
const MAX_RUNS: usize = 4096;
const TAB_PAD: f32 = 6.0;

const PALETTE: [[f32; 3]; 16] = [
    [0.0, 0.0, 0.0], [0.8, 0.2, 0.2], [0.2, 0.6, 0.2], [0.6, 0.6, 0.2],
    [0.2, 0.2, 0.8], [0.6, 0.2, 0.6], [0.2, 0.6, 0.6], [0.8, 0.8, 0.8],
    [0.4, 0.4, 0.4], [1.0, 0.4, 0.4], [0.4, 1.0, 0.4], [1.0, 1.0, 0.4],
    [0.4, 0.4, 1.0], [1.0, 0.4, 1.0], [0.4, 1.0, 1.0], [1.0, 1.0, 1.0],
];

static TRACE_T0: OnceLock<Instant> = OnceLock::new();

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

struct Run {
    row: u32,
    col: u32,
    width: u32,
    fg: [f32; 3],
    bg: [f32; 3],
    text: String,
}

struct FrameData {
    runs: Vec<Run>,
    cursor: Option<(u32, u32)>,
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
    tablist_hidden: bool,
    fg: [f32; 3],
    bg: [f32; 3],
    wake: Arc<AtomicBool>,
    exited: Arc<Mutex<Vec<usize>>>,
    window_slot: Arc<Mutex<Option<Arc<Window>>>>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    font_system: FontSystem,
    mono_attrs: Attrs<'static>,
    tab_attrs: Attrs<'static>,
    modifiers: ModifiersState,
    program: String,
    args: Vec<String>,
    /// Test hook: receiver of key specs fed from the $SPIKE_KEYS FIFO by a
    /// reader thread (about_to_wait is rarely called on Wayland).
    key_rx: Option<std::sync::mpsc::Receiver<String>>,
}

impl App {
    fn new() -> Self {
        let mut font_system = FontSystem::new();
        let config = Config::load();
        // Pango sizes are points; cosmic-text wants pixels (@96 dpi).
        let (term_family, term_pt) = config::parse_font(&config.term_font);
        let (tab_family, tab_pt) = config::parse_font(&config.tablist_font);
        let font_px = term_pt * 96.0 / 72.0;
        let tab_font_px = tab_pt * 96.0 / 72.0;
        // Font size comes from the Buffer Metrics (see Gpu::init), not Attrs.
        // mono_attrs/tab_attrs are 'static, so the family names must be too.
        let term_family: &'static str = Box::leak(term_family.into_boxed_str());
        let tab_family: &'static str = Box::leak(tab_family.into_boxed_str());
        let mono_attrs = Attrs::new().family(Family::Name(term_family));
        let tab_attrs = Attrs::new().family(Family::Name(tab_family));

        let mut measure = Buffer::new(&mut font_system, Metrics::new(font_px, font_px * 1.3));
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
        let cell_w = (total / 4.0).max(1.0);
        let cell_h = font_px * 1.3;

        let mut tmeasure =
            Buffer::new(&mut font_system, Metrics::new(tab_font_px, tab_font_px * 1.3));
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
        let tab_h = tab_font_px * 1.3 + 6.0;

        log::info!(
            "font: term {term_family} {term_pt}pt, tablist {tab_family} {tab_pt}pt; cell = {cell_w:.2} x {cell_h:.2}, tab char = {tab_char_w:.2}"
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
            wake: Arc::new(AtomicBool::new(false)),
            window: Arc::clone(&window_slot),
            tab_id: 0,
            title: Arc::new(Mutex::new(String::new())),
            exited: Arc::new(Mutex::new(Vec::new())),
        };
        let wake = Arc::clone(&shared.wake);
        let exited = Arc::clone(&shared.exited);

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
            tab_text_h: tab_font_px * 1.3,
            tablist_hidden,
            fg,
            bg,
            wake,
            exited,
            window_slot,
            window: None,
            gpu: None,
            font_system,
            mono_attrs,
            tab_attrs,
            modifiers: ModifiersState::empty(),
            program,
            args,
            key_rx,
        }
    }

    fn init_gpu(&mut self, event_loop: &ActiveEventLoop, window: &Arc<Window>) {
        let (_, term_pt) = config::parse_font(&self.config.term_font);
        let (_, tab_pt) = config::parse_font(&self.config.tablist_font);
        self.gpu = Some(Gpu::init(
            event_loop,
            window,
            &mut self.font_system,
            term_pt * 96.0 / 72.0,
            tab_pt * 96.0 / 72.0,
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
            wake: Arc::clone(&self.wake),
            window: Arc::clone(&self.window_slot),
            tab_id: 0,
            title: Arc::new(Mutex::new(String::new())),
            exited: Arc::clone(&self.exited),
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
        let (ncols, nrows) =
            ((w as f32 / self.cell_w) as usize, (term_h as f32 / self.cell_h) as usize);
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

    /// Recompute font-derived metrics and rebuild the glyph pools. The base
    /// Metrics (line height / scale) are baked into each pooled Buffer at
    /// creation, so a size or family change requires new buffers.
    fn apply_fonts(&mut self) {
        let (term_family, term_pt) = config::parse_font(&self.config.term_font);
        let (tab_family, tab_pt) = config::parse_font(&self.config.tablist_font);
        let font_px = term_pt * 96.0 / 72.0;
        let tab_font_px = tab_pt * 96.0 / 72.0;

        let term_family: &'static str = Box::leak(term_family.into_boxed_str());
        let tab_family: &'static str = Box::leak(tab_family.into_boxed_str());
        self.mono_attrs = Attrs::new().family(Family::Name(term_family));
        self.tab_attrs = Attrs::new().family(Family::Name(tab_family));

        // Measure the terminal cell from the new font.
        let mut measure =
            Buffer::new(&mut self.font_system, Metrics::new(font_px, font_px * 1.3));
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
        let cell_w = (total / 4.0).max(1.0);
        let cell_h = font_px * 1.3;

        // Measure the tablist character width.
        let mut tmeasure =
            Buffer::new(&mut self.font_system, Metrics::new(tab_font_px, tab_font_px * 1.3));
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
        let tab_h = tab_font_px * 1.3 + 6.0;

        self.cell_w = cell_w;
        self.cell_h = cell_h;
        self.tab_char_w = tab_char_w;
        self.tab_h = tab_h;
        self.tab_text_h = tab_font_px * 1.3;

        // Rebuild the glyph pools with the new base metrics.
        if let Some(gpu) = &mut self.gpu {
            gpu.pool = (0..MAX_RUNS)
                .map(|_| Buffer::new(&mut self.font_system, Metrics::new(font_px, font_px * 1.3)))
                .collect();
            gpu.tab_pool = (0..16)
                .map(|_| {
                    Buffer::new(
                        &mut self.font_system,
                        Metrics::new(tab_font_px, tab_font_px * 1.3),
                    )
                })
                .collect();
        }

        // Cell size changed -> the terminal grid that fits the surface changes.
        self.sync_term_size();

        log::info!(
            "reload fonts: term {term_family} {term_pt}pt, tablist {tab_family} {tab_pt}pt; cell = {cell_w:.2} x {cell_h:.2}, tab char = {tab_char_w:.2}"
        );
    }

    fn build_frame(&self, tab: &Tab) -> FrameData {
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

            let mut glyph = String::with_capacity(4);
            glyph.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                glyph.extend(zw.iter());
            }

            let fg = palette.resolve_with(&cell.fg, self.fg, self.bg);
            let bg = palette.resolve_with(&cell.bg, self.fg, self.bg);

            match cur.as_mut() {
                Some(r)
                    if r.row == row as u32
                        && r.col + r.width == col as u32
                        && r.fg == fg
                        && r.bg == bg =>
                {
                    r.width += 1;
                    r.text.push_str(&glyph);
                },
                _ => {
                    if let Some(r) = cur.take() {
                        runs.push(r);
                    }
                    cur = Some(Run {
                        row: row as u32,
                        col: col as u32,
                        width: 1,
                        fg,
                        bg,
                        text: glyph,
                    });
                },
            }
        }
        if let Some(r) = cur.take() {
            runs.push(r);
        }

        let cursor = match content.cursor.shape {
            alacritty_terminal::vte::ansi::CursorShape::Hidden => None,
            _ => Some((
                content.cursor.point.column.0 as u32,
                content.cursor.point.line.0 as u32,
            )),
        };

        FrameData { runs, cursor }
    }

    fn draw(&mut self, window: &Window) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        // The active tab is "focused" iff the window has input focus
        // (covers tab switching, which produces no Focused event).
        {
            let active = &self.tabs[self.active];
            active.term.lock().is_focused = window.has_focus();
        }
        let frame = self.build_frame(tab);
        let show_strip = self.show_tab_strip();
        let gpu: &mut Gpu = match self.gpu.as_mut() {
            Some(g) => g,
            None => return,
        };
        let (w, h) = (gpu.config.width, gpu.config.height);
        if w == 0 || h == 0 {
            return;
        }
        let wf = w as f32;
        let hf = h as f32;

        let strip_h = if show_strip { self.tab_h } else { 0.0 };

        gpu.rects.clear();
        // Convert pixel coords to clip space. Clip is -1..1 on both axes with
        // +y pointing up, screen pixels have y pointing down, so flip y.
        let push_rect =
            |v: &mut Vec<gpu::RectVertex>, x: f32, y: f32, rw: f32, rh: f32, c: [f32; 4]| {
                let clip = |px: f32, py: f32| -> [f32; 2] {
                    [(px / wf) * 2.0 - 1.0, 1.0 - (py / hf) * 2.0]
                };
                v.push(gpu::RectVertex { pos: clip(x, y), color: c });
                v.push(gpu::RectVertex { pos: clip(x + rw, y), color: c });
                v.push(gpu::RectVertex { pos: clip(x, y + rh), color: c });
                v.push(gpu::RectVertex { pos: clip(x, y + rh), color: c });
                v.push(gpu::RectVertex { pos: clip(x + rw, y), color: c });
                v.push(gpu::RectVertex { pos: clip(x + rw, y + rh), color: c });
            };

        // Window background
        push_rect(&mut gpu.rects, 0.0, 0.0, w as f32, h as f32, [self.bg[0], self.bg[1], self.bg[2], 1.0]);

        // --- Text areas (tab labels + terminal runs), one prepare() per frame ---
        let mut areas: Vec<TextArea> = Vec::new();
        let bounds = TextBounds { left: 0, top: 0, right: w as i32, bottom: h as i32 };

        // --- Tab strip ---
        let mut x = 0.0f32;
        let tab_y = 0.0f32;
        if show_strip {
            let bg4: [f32; 4] = [self.bg[0], self.bg[1], self.bg[2], 1.0];
            push_rect(&mut gpu.rects, 0.0, 0.0, w as f32, self.tab_h, bg4);
            // SAFETY: same pool reasoning as for terminal runs below.
            {
                let fs = &mut self.font_system;
                let attrs = &self.tab_attrs;
                let tpool = &mut gpu.tab_pool;
                // Each label needs its own pool slot. An entangled tab takes a
                // second slot for the red * marker (two areas sharing one
                // Buffer would both draw the buffer's current contents).
                let usable = tpool.len() - 1; // reserve the last slot for [+]
                let mut slot = 0usize;
                for (i, t) in self.tabs.iter().enumerate() {
                    let need = if t.entangled { 2 } else { 1 };
                    if slot + need > usable {
                        break;
                    }
                    let marker_slot = slot;
                    let label_slot = slot + (if t.entangled { 1 } else { 0 });
                    let selected = i == self.active;
                    let (fgc, bgc) = if selected {
                        (self.config.tab_color_fg_selected.to_f32(), self.config.tab_color_bg_selected.to_f32())
                    } else {
                        (self.config.tab_color_fg.to_f32(), self.config.tab_color_bg.to_f32())
                    };
                    let fgc = if t.group < 8 { self.config.col[t.group].to_f32() } else { fgc };
                    let text = tab_label(
                        t,
                        i,
                        selected,
                        self.config.tab_title_max_length,
                        self.config.tab_title_short_current,
                        &self.config.tab_shorten_str,
                    );
                    let marker_w = if t.entangled { self.tab_char_w } else { 0.0 };
                    let tw = text.chars().count() as f32 * self.tab_char_w + marker_w + TAB_PAD * 2.0;
                    let tbg = if selected { bgc } else { self.config.tab_color_bg.to_f32() };
                    push_rect(&mut gpu.rects, x, tab_y, tw, self.tab_h, [tbg[0], tbg[1], tbg[2], 1.0]);
                    let ty = tab_y + (self.tab_h - self.tab_text_h) / 2.0;
                    if t.entangled {
                        push_tab_label(tpool, marker_slot, fs, attrs, "*", x + TAB_PAD, ty, self.config.tab_color_entangle.to_f32(), &mut areas, self.tab_char_w, bounds);
                    }
                    push_tab_label(tpool, label_slot, fs, attrs, &text, x + TAB_PAD + marker_w, ty, fgc, &mut areas, self.tab_char_w, bounds);
                    slot += need;
                    x += tw;
                }
                // [+] add button
                let bw = 2.0 * self.tab_char_w + TAB_PAD * 2.0;
                let c = self.config.add_button_color.to_f32();
                push_rect(&mut gpu.rects, x, tab_y, bw, self.tab_h, [c[0] * 0.3, c[1] * 0.3, c[2] * 0.3, 1.0]);
                if slot < tpool.len() {
                    push_tab_label(tpool, slot, fs, attrs, "[+]", x + TAB_PAD, tab_y + (self.tab_h - self.tab_text_h) / 2.0, c, &mut areas, self.tab_char_w, bounds);
                }
            }
        }

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
        // Cursor
        if let Some((cx, cy)) = frame.cursor {
            push_rect(
                &mut gpu.rects,
                cx as f32 * self.cell_w,
                strip_h + cy as f32 * self.cell_h,
                self.cell_w,
                self.cell_h,
                [CURSOR_COLOR[0], CURSOR_COLOR[1], CURSOR_COLOR[2], 0.35],
            );
        }

        // --- Terminal text ---
        // SAFETY: pool indices are disjoint (run index < MAX_RUNS), the pool
        // never reallocates after init, and the derived shared references are
        // only used this frame, before any further pool writes.
        {
            let fs = &mut self.font_system;
            let attrs = &self.mono_attrs;
            let pool = &mut gpu.pool;
            for (i, run) in frame.runs.iter().take(MAX_RUNS).enumerate() {
                let blank = run.text.chars().all(|c| c == ' ');
                let default_fg =
                    (run.fg[0] - self.fg[0]).abs() + (run.fg[1] - self.fg[1]).abs()
                        + (run.fg[2] - self.fg[2]).abs()
                        < 0.01;
                if blank && default_fg {
                    continue;
                }
                let b = &mut pool[i];
                b.set_size(Some(run.width as f32 * self.cell_w), None);
                b.set_text(&run.text, attrs, Shaping::Basic, None);
                b.set_monospace_width(Some(self.cell_w));
                b.shape_until_scroll(fs, false);
                let ptr: *const Buffer = b;
                let buf: &Buffer = unsafe { &*ptr };
                areas.push(TextArea {
                    buffer: buf,
                    left: run.col as f32 * self.cell_w,
                    top: strip_h + run.row as f32 * self.cell_h,
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

        gpu.render_frame(&mut self.font_system, areas, window);

        if std::env::var("SPIKE_TRACE").is_ok() {
            let t0 = *TRACE_T0.get_or_init(Instant::now);
            let mut h: u64 = 0xcbf29ce484222325;
            for run in &frame.runs {
                for b in run.text.as_bytes() {
                    h ^= *b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
            eprintln!("{:?} {} {:?}", Instant::now().duration_since(t0), h, frame.cursor);
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
                log::info!("SPY active={} surf={} cursor=[{}] tabs=[{}]", self.active, surf, cursor, state.join(","));
                continue;
            }
            if let Some((mods, key)) = parse_key_spec(line) {
                self.modifiers = mods;
                self.handle_key(&key, event_loop);
            }
        }
        if let Some(window) = self.window.as_ref() {
            self.wake.store(true, Ordering::SeqCst);
            window.request_redraw();
        }
    }

    fn handle_key(&mut self, key: &Key, event_loop: &ActiveEventLoop) {
        // 1. Configured bindings
        let overview_active = false; // overview not ported yet
        if let Some(action) = config::find_action(&self.config.bindings, key, self.modifiers, overview_active) {
            self.do_action(&action, event_loop);
            return;
        }
        // 2. Everything else goes to the active terminal
        if let Some(bytes) = key_bytes(key, self.modifiers) {
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
            // Not yet ported
            "show_search" | "show_search_all" | "search_prev" | "search_next"
            | "show_tab_overview" | "rename_tab" | "shrink_font" | "enlarge_font" => {
                log::debug!("unported action: {action}");
            },
            _ => {},
        }
        self.wake.store(true, Ordering::SeqCst);
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// Build the tab strip label text, mimicking the Python tab_format.
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
                if self.wake.swap(false, Ordering::SeqCst) {
                    self.draw(&window);
                }
            },
            WindowEvent::KeyboardInput { event: key, .. } => {
                if key.state == ElementState::Pressed && !key.repeat {
                    self.handle_key(&key.logical_key, event_loop);
                }
            },
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            },
            WindowEvent::Focused(focused) => {
                if let Some(tab) = self.tabs.get(self.active) {
                    tab.term.lock().is_focused = focused;
                }
                self.wake.store(true, Ordering::SeqCst);
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
                self.wake.store(true, Ordering::SeqCst);
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

fn main() {
    env_logger::init();
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App::new();
    event_loop.run_app(&mut app).expect("run");
    log::info!("exited");
}
