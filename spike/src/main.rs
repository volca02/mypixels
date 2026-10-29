//! Spike: alacritty_terminal (emulation + pty + reader event loop) rendered
//! with glyphon (cosmic_text) on wgpu in a winit window.
//!
//! Goal: measure redraw performance under agentic-style full-screen redraw
//! storms compared to VTE (mypixels), alacritty, and ghostty.
//!
//! Usage: spike [command [args...]]   (defaults to bash)

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

static TRACE_T0: OnceLock<std::time::Instant> = OnceLock::new();
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options, Shell};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use glyphon::{
    Attrs, Buffer, Cache, Color as GColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const FONT_PX: f32 = 15.0;
const HISTORY: usize = 10_000;
const MAX_RUNS: usize = 4096;

const PALETTE: [[f32; 3]; 16] = [
    [0.0, 0.0, 0.0], [0.8, 0.2, 0.2], [0.2, 0.6, 0.2], [0.6, 0.6, 0.2],
    [0.2, 0.2, 0.8], [0.6, 0.2, 0.6], [0.2, 0.6, 0.6], [0.8, 0.8, 0.8],
    [0.4, 0.4, 0.4], [1.0, 0.4, 0.4], [0.4, 1.0, 0.4], [1.0, 1.0, 0.4],
    [0.4, 0.4, 1.0], [1.0, 0.4, 1.0], [0.4, 1.0, 1.0], [1.0, 1.0, 1.0],
];

const DEFAULT_FG: [f32; 3] = [0.85, 0.85, 0.85];
const DEFAULT_BG: [f32; 3] = [0.12, 0.12, 0.14];
const CURSOR_COLOR: [f32; 3] = [0.55, 0.55, 0.6];

fn cube_color(i: u8) -> [f32; 3] {
    let i = (i - 16) as usize;
    let (r, g, b) = (i / 36, (i / 6) % 6, i % 6);
    let c = |v: usize| if v == 0 { 0.0 } else { (95.0 + 40.0 * v as f32) / 255.0 };
    [c(r), c(g), c(b)]
}

#[derive(Clone)]
struct Palette {
    colors: Box<[Option<[f32; 3]>]>,
}

impl Palette {
    fn from_colors(colors: &Colors) -> Self {
        let mut v = Vec::with_capacity(269);
        for i in 0..269 {
            v.push(match &colors[i] {
                Some(rgb) => Some([rgb.r as f32 / 255.0, rgb.g as f32 / 255.0, rgb.b as f32 / 255.0]),
                None => None,
            });
        }
        Self { colors: v.into() }
    }

    fn resolve(&self, c: &Color) -> [f32; 3] {
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
                    PALETTE[i]
                } else if i == NamedColor::Foreground as usize {
                    DEFAULT_FG
                } else if i == NamedColor::Background as usize {
                    DEFAULT_BG
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
                    PALETTE[*i as usize]
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
// Dimensions
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Event listener (runs on the terminal event loop thread)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SpikeListener {
    wake: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    window: Arc<Mutex<Option<Arc<Window>>>>,
}

impl SpikeListener {
    fn notify_window(&self) {
        if let Some(w) = self.window.lock().unwrap().as_ref() {
            w.request_redraw();
        }
    }
}

impl EventListener for SpikeListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                self.wake.store(true, Ordering::SeqCst);
                self.notify_window();
            },
            Event::Title(t) => {
                log::info!("title: {t}");
            },
            Event::Exit | Event::ChildExit(_) => {
                self.quit.store(true, Ordering::SeqCst);
                self.wake.store(true, Ordering::SeqCst);
                self.notify_window();
            },
            _ => {},
        }
    }
}

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
// GPU state
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct RectVertex {
    pos: [f32; 2],
    color: [f32; 4],
}

const RECT_VS_WGSL: &str = r#"
struct VSOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};
@vertex
fn vs(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>) -> VSOut {
    return VSOut(vec4<f32>(pos, 0.0, 1.0), color);
}
"#;

const RECT_FS_WGSL: &str = r#"
@fragment
fn fs(@location(0) color: vec4<f32>) -> @location(0) vec4<f32> {
    return color;
}
"#;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    rect_pipeline: wgpu::RenderPipeline,
    rect_buffer: wgpu::Buffer,
    viewport: Viewport,
    atlas: TextAtlas,
    swash_cache: SwashCache,
    text_renderer: TextRenderer,
    rects: Vec<RectVertex>,
    pool: Vec<Buffer>,
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn raw_waker() -> RawWaker {
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            raw_waker()
        }
        RawWaker::new(std::ptr::null(), &RawWakerVTable::new(clone, noop, noop, noop))
    }
    let waker = unsafe { Waker::from_raw(raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct App {
    term: Arc<FairMutex<Term<SpikeListener>>>,
    sender: EventLoopSender,
    wake: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    term_loop: std::thread::JoinHandle<(
        alacritty_terminal::event_loop::EventLoop<alacritty_terminal::tty::Pty, SpikeListener>,
        alacritty_terminal::event_loop::State,
    )>,
    window_slot: Arc<Mutex<Option<Arc<Window>>>>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    font_system: FontSystem,
    mono_attrs: Attrs<'static>,
    modifiers: winit::keyboard::ModifiersState,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
}

impl App {
    fn new() -> Self {
        let mut font_system = FontSystem::new();
        let mono_attrs = Attrs::new().family(Family::Monospace);

        let mut measure =
            Buffer::new(&mut font_system, Metrics::new(FONT_PX, FONT_PX * 1.3));
        measure.set_size(Some(800.0), None);
        measure.set_text("MMMM", &mono_attrs, Shaping::Basic, None);
        measure.shape_until_scroll(&mut font_system, false);
        let shaped = measure.lines[0].shape(&mut font_system, 8);
        let mut total = 0.0;
        for span in &shaped.spans {
            for word in &span.words {
                total += word.width(FONT_PX);
            }
        }
        let cell_w = (total / 4.0).max(1.0);
        let cell_h = FONT_PX * 1.3;
        log::info!("cell = {cell_w:.2} x {cell_h:.2} @ {FONT_PX}px monospace");

        let cols = 100;
        let rows = 30;
        let window_slot = Arc::new(Mutex::new(None));
        let listener = SpikeListener {
            wake: Arc::new(AtomicBool::new(false)),
            quit: Arc::new(AtomicBool::new(false)),
            window: Arc::clone(&window_slot),
        };
        let wake = Arc::clone(&listener.wake);
        let quit = Arc::clone(&listener.quit);

        let term = Arc::new(FairMutex::new(Term::new(
            TermConfig::default(),
            &Dims { cols, rows },
            listener.clone(),
        )));

        let args: Vec<String> = std::env::args().collect();
        let (program, program_args) = if args.len() > 1 {
            (args[1].clone(), args[2..].to_vec())
        } else {
            ("bash".to_string(), vec![])
        };

        tty::setup_env();
        let pty = tty::new(
            &Options {
                shell: Some(Shell::new(program, program_args)),
                working_directory: None,
                drain_on_exit: false,
                env: HashMap::new(),
            },
            WindowSize {
                num_lines: rows as u16,
                num_cols: cols as u16,
                cell_width: cell_w as u16,
                cell_height: cell_h as u16,
            },
            0,
        )
        .expect("failed to open pty");

        let loop_ = alacritty_terminal::event_loop::EventLoop::new(
            Arc::clone(&term),
            listener.clone(),
            pty,
            false,
            false,
        )
        .expect("terminal event loop");
        let sender = loop_.channel();
        let term_loop = loop_.spawn();

        App {
            term,
            sender,
            wake,
            quit,
            term_loop,
            window_slot,
            window: None,
            gpu: None,
            font_system,
            mono_attrs,
            modifiers: winit::keyboard::ModifiersState::empty(),
            cell_w,
            cell_h,
            cols,
            rows,
        }
    }

    fn init_gpu(&mut self, event_loop: &ActiveEventLoop, window: &Arc<Window>) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(event_loop.owned_display_handle()),
        ));
        let surface = instance.create_surface(window.clone()).expect("surface");
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .expect("no adapter");
        log::info!("adapter: {}", adapter.get_info().name);
        let (device, queue) =
            block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("no device");

        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let (width, height) = (window.inner_size().width.max(1), window.inner_size().height.max(1));
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Mailbox,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let cache = Cache::new(&device);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let swash_cache = SwashCache::new();
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let mut viewport = Viewport::new(&device, &cache);
        viewport.update(&queue, Resolution { width, height });

        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rects"),
            layout: Some(&device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor::default())),
            vertex: wgpu::VertexState {
                module: &device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("rect.vs"),
                    source: wgpu::ShaderSource::Wgsl(RECT_VS_WGSL.into()),
                }),
                compilation_options: Default::default(),
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<RectVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            offset: 0,
                            shader_location: 0,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        wgpu::VertexAttribute {
                            offset: 8,
                            shader_location: 1,
                            format: wgpu::VertexFormat::Float32x4,
                        },
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("rect.fs"),
                    source: wgpu::ShaderSource::Wgsl(RECT_FS_WGSL.into()),
                }),
                compilation_options: Default::default(),
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let rect_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rects"),
            size: (1024 * 24) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let pool = (0..MAX_RUNS)
            .map(|_| Buffer::new(&mut self.font_system, Metrics::new(FONT_PX, self.cell_h)))
            .collect();

        self.gpu = Some(Gpu {
            device,
            queue,
            surface,
            config,
            rect_pipeline,
            rect_buffer,
            viewport,
            atlas,
            swash_cache,
            text_renderer,
            rects: Vec::with_capacity(1024),
            pool,
        });
    }

    // Copy visible terminal content into an owned draw list.
    fn build_frame(&self) -> FrameData {
        let term = self.term.lock();
        let content = term.renderable_content();
        let palette = Palette::from_colors(content.colors);
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

            let fg = palette.resolve(&cell.fg);
            let bg = palette.resolve(&cell.bg);

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
        let frame = self.build_frame();
        let gpu = match self.gpu.as_mut() {
            Some(g) => g,
            None => return,
        };
        let (w, h) = (window.inner_size().width, window.inner_size().height);
        if w == 0 || h == 0 {
            return;
        }

        if w != gpu.config.width || h != gpu.config.height {
            gpu.config.width = w;
            gpu.config.height = h;
            gpu.surface.configure(&gpu.device, &gpu.config);
            gpu.viewport.update(&gpu.queue, Resolution { width: w, height: h });

            let (ncols, nrows) = ((w as f32 / self.cell_w) as usize, (h as f32 / self.cell_h) as usize);
            if (ncols, nrows) != (self.cols, self.rows) {
                self.cols = ncols;
                self.rows = nrows;
                let dims = Dims { cols: ncols, rows: nrows };
                self.term.lock().resize(dims);
                let _ = self.sender.send(Msg::Resize(WindowSize {
                    num_lines: nrows as u16,
                    num_cols: ncols as u16,
                    cell_width: self.cell_w as u16,
                    cell_height: self.cell_h as u16,
                }));
            }
        }

        // --- Rects: full background, cell backgrounds, cursor ---
        gpu.rects.clear();
        let mut push_rect = |v: &mut Vec<RectVertex>, x: f32, y: f32, rw: f32, rh: f32, c: [f32; 4]| {
            v.push(RectVertex { pos: [x, y], color: c });
            v.push(RectVertex { pos: [x + rw, y], color: c });
            v.push(RectVertex { pos: [x, y + rh], color: c });
            v.push(RectVertex { pos: [x, y + rh], color: c });
            v.push(RectVertex { pos: [x + rw, y], color: c });
            v.push(RectVertex { pos: [x + rw, y + rh], color: c });
        };
        let bg4: [f32; 4] = [DEFAULT_BG[0], DEFAULT_BG[1], DEFAULT_BG[2], 1.0];
        push_rect(&mut gpu.rects, 0.0, 0.0, w as f32, h as f32, bg4);

        for run in &frame.runs {
            let same_bg = (run.bg[0] - DEFAULT_BG[0]).abs() < 0.004
                && (run.bg[1] - DEFAULT_BG[1]).abs() < 0.004
                && (run.bg[2] - DEFAULT_BG[2]).abs() < 0.004;
            if !same_bg {
                push_rect(
                    &mut gpu.rects,
                    run.col as f32 * self.cell_w,
                    run.row as f32 * self.cell_h,
                    run.width as f32 * self.cell_w,
                    self.cell_h,
                    [run.bg[0], run.bg[1], run.bg[2], 1.0],
                );
            }
        }

        if let Some((cx, cy)) = frame.cursor {
            push_rect(
                &mut gpu.rects,
                cx as f32 * self.cell_w,
                cy as f32 * self.cell_h,
                self.cell_w,
                self.cell_h,
                [CURSOR_COLOR[0], CURSOR_COLOR[1], CURSOR_COLOR[2], 0.35],
            );
        }

        // --- Text: one pooled buffer per run ---
        // SAFETY: pool indices are disjoint (run index < MAX_RUNS, no aliasing),
        // the pool Vec never reallocates after init, and the derived shared
        // references are only used this frame, before any further pool writes.
        let mut areas: Vec<TextArea> = Vec::new();
        {
            let fs = &mut self.font_system;
            let attrs = &self.mono_attrs;
            let pool = &mut gpu.pool;
            for (i, run) in frame.runs.iter().take(MAX_RUNS).enumerate() {
                let blank = run.text.chars().all(|c| c == ' ');
                let default_fg = (run.fg[0] - DEFAULT_FG[0]).abs() + (run.fg[1] - DEFAULT_FG[1]).abs()
                    + (run.fg[2] - DEFAULT_FG[2]).abs() < 0.01;
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
                    top: run.row as f32 * self.cell_h,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: w as i32,
                        bottom: h as i32,
                    },
                    default_color: GColor::rgb(
                        (run.fg[0] * 255.0) as u8,
                        (run.fg[1] * 255.0) as u8,
                        (run.fg[2] * 255.0) as u8,
                    ),
                    custom_glyphs: &[],
                });
            }
        }

        // Debug: log a hash of what we drew each frame (set SPIKE_TRACE to enable)
        if std::env::var("SPIKE_TRACE").is_ok() {
            let mut h: u64 = 0xcbf29ce484222325;
            for run in &frame.runs {
                for b in run.text.as_bytes() {
                    h ^= *b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
            let t0 = *TRACE_T0.get_or_init(std::time::Instant::now);
            eprintln!("{:?} {} {:?}", std::time::Instant::now().duration_since(t0), h, frame.cursor);
        }

        // --- Render ---
        let frame_tex = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                // Texture is still usable. Present it; Resized handler will
                // reconfigure the surface if the window actually changed size.
                f
            },
            other => {
                // No texture right now (compositor busy / occluded). Try again.
                if matches!(other, wgpu::CurrentSurfaceTexture::Timeout) {
                    log::debug!("surface timeout, will retry");
                }
                window.request_redraw();
                return;
            },
        };
        let view = frame_tex.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            pass.set_pipeline(&gpu.rect_pipeline);
            if gpu.rects.len() * 24 > gpu.rect_buffer.size() as usize {
                gpu.rect_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("rects"),
                    size: (gpu.rects.len() * 48) as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            gpu.queue
                .write_buffer(&gpu.rect_buffer, 0, bytemuck::cast_slice(&gpu.rects));
            pass.set_vertex_buffer(0, gpu.rect_buffer.slice(..));
            pass.draw(0..gpu.rects.len() as u32, 0..1);

            gpu.text_renderer
                .prepare(
                    &gpu.device,
                    &gpu.queue,
                    &mut self.font_system,
                    &mut gpu.atlas,
                    &gpu.viewport,
                    areas,
                    &mut gpu.swash_cache,
                )
                .expect("prepare");
            gpu.text_renderer
                .render(&gpu.atlas, &gpu.viewport, &mut pass)
                .expect("render");
        }
        gpu.queue.submit(Some(encoder.finish()));
        gpu.queue.present(frame_tex);
    }

    fn write_pty(&self, bytes: &[u8]) {
        let _ = self.sender.send(Msg::Input(Cow::Owned(bytes.to_vec())));
    }
}

fn key_bytes(k: &winit::event::KeyEvent, mods: winit::keyboard::ModifiersState) -> Option<Vec<u8>> {
    if k.state != winit::event::ElementState::Pressed {
        return None;
    }
    use winit::keyboard::ModifiersState;
    let ctrl = mods.contains(ModifiersState::CONTROL);
    let alt = mods.contains(ModifiersState::ALT);
    let esc_seq = |body: &[u8]| -> Vec<u8> {
        let mut v = vec![0x1b];
        v.extend_from_slice(body);
        v
    };
    match &k.logical_key {
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
                (self.rows as f32 * self.cell_h) as u32,
            );
            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_inner_size(size)
                            .with_title("spike"),
                    )
                    .expect("window"),
            );
            self.init_gpu(event_loop, &window);
            *self.window_slot.lock().unwrap() = Some(Arc::clone(&window));
            self.window = Some(window);
        }
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
                if self.wake.swap(false, Ordering::SeqCst) {
                    self.draw(&window);
                }
                if self.quit.load(Ordering::SeqCst) {
                    event_loop.exit();
                }
            },
            WindowEvent::KeyboardInput { event: key, .. } => {
                if let Some(bytes) = key_bytes(&key, self.modifiers) {
                    self.write_pty(&bytes);
                }
            },
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            },
            WindowEvent::Focused(focused) => {
                self.term.lock().is_focused = focused;
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

                        let (ncols, nrows) =
                            ((size.width as f32 / self.cell_w) as usize,
                             (size.height as f32 / self.cell_h) as usize);
                        if (ncols, nrows) != (self.cols, self.rows) {
                            self.cols = ncols;
                            self.rows = nrows;
                            let dims = Dims { cols: ncols, rows: nrows };
                            self.term.lock().resize(dims);
                            let _ = self.sender.send(Msg::Resize(WindowSize {
                                num_lines: nrows as u16,
                                num_cols: ncols as u16,
                                cell_width: self.cell_w as u16,
                                cell_height: self.cell_h as u16,
                            }));
                        }
                    }
                }
                self.wake.store(true, Ordering::SeqCst);
                window.request_redraw();
            },
            _ => {},
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

fn main() {
    env_logger::init();
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App::new();
    event_loop.run_app(&mut app).expect("run");
    log::info!("exited");
}
