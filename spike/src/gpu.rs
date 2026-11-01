//! GPU state: wgpu device/surface, rect pipeline (backgrounds, cursor,
//! tab strip), glyphon text rendering.

use std::sync::Arc;
use std::time::Instant;

use glyphon::{Buffer, Cache, ColorMode, FontSystem, Metrics, Resolution, SwashCache, TextAtlas, TextArea, TextRenderer, Viewport};
use winit::window::Window;

use crate::MAX_RUNS;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct RectVertex {
    pub pos: [f32; 2],
    pub color: [f32; 4],
    /// Offset from this rect's top-left in pixels. The fragment shader needs a
    /// local coordinate to evaluate a corner radius.
    pub local: [f32; 2],
    /// (width, height, corner radius, unused) in pixels.
    pub size_radius: [f32; 4],
}

/// One axis-aligned rect as six vertices -- two triangles, since the pipeline
/// is TriangleList. Coordinates are pixels, converted to clip space against a
/// `wf` x `hf` window. `radius` rounds the corners with an antialiased distance
/// field; 0.0 gives a plain rect.
///
/// Vertex order is (TL, TR, BR) + (TL, BR, BL). An earlier version emitted
/// (TL, TR, BL, BR, BL, BR), whose second triangle is degenerate, so only half
/// of every rect filled -- invisible on small swatches, obvious on big ones.
pub fn rect_vertices(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: [f32; 4],
    radius: f32,
    wf: f32,
    hf: f32,
) -> [RectVertex; 6] {
    let clip = |px: f32, py: f32| -> [f32; 2] {
        [(px / wf) * 2.0 - 1.0, 1.0 - (py / hf) * 2.0]
    };
    let sr = [w, h, radius.min(w.min(h) * 0.5), 0.0];
    let v = |px: f32, py: f32| RectVertex {
        pos: clip(px, py),
        color,
        local: [px - x, py - y],
        size_radius: sr,
    };
    let (x1, y1) = (x + w, y + h);
    [
        v(x, y),
        v(x1, y),
        v(x1, y1),
        v(x, y),
        v(x1, y1),
        v(x, y1),
    ]
}

const RECT_VS_WGSL: &str = r#"
struct VSOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local: vec2<f32>,
    @location(2) size_radius: vec4<f32>,
};
@vertex
fn vs(@location(0) pos: vec2<f32>,
      @location(1) color: vec4<f32>,
      @location(2) local: vec2<f32>,
      @location(3) size_radius: vec4<f32>) -> VSOut {
    return VSOut(vec4<f32>(pos, 0.0, 1.0), color, local, size_radius);
}
"#;

const RECT_FS_WGSL: &str = r#"
@fragment
fn fs(@location(0) color: vec4<f32>,
      @location(1) local: vec2<f32>,
      @location(2) size_radius: vec4<f32>) -> @location(0) vec4<f32> {
    let r = size_radius.z;
    if (r <= 0.0) {
        return color;
    }
    // Signed distance to a rounded box; negative is inside.
    let half = size_radius.xy * 0.5;
    let q = abs(local - half) - half + vec2<f32>(r);
    let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
    return vec4<f32>(color.rgb, color.a * smoothstep(0.5, -0.5, d));
}
"#;

/// The tab-switcher overlay, drawn above the live terminal. Its dim and tile
/// rects plus tile text form a second phase after the terminal's own text
/// pass, so the terminal stays visible (dimmed) behind the switcher. Without
/// the split, the single text pass would paint terminal text over the opaque
/// tiles -- and the shared glyph pool would let the tile headers clobber the
/// terminal runs they alias.
///
/// The rects draw through `overlay_rect_pipeline`, whose "src over" alpha
/// equation never drops the window's own alpha -- the dim darkens the
/// terminal, it does not make the window more transparent.
pub struct OverlayPhase<'a> {
    pub rects: Vec<RectVertex>,
    pub areas: Vec<TextArea<'a>>,
}

/// Stage `rects` into `buffer` (growing it if needed) and draw them through
/// `pipeline`. Each call site owns its buffer: the base and the switcher
/// phases must never share one, because both writes land at offset 0 and the
/// draws only execute at the frame's single submit -- a shared buffer would
/// make both draws read the last write.
fn draw_rects(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pass: &mut wgpu::RenderPass<'_>,
    buffer: &mut wgpu::Buffer,
    pipeline: &wgpu::RenderPipeline,
    rects: &[RectVertex],
) {
    if rects.is_empty() {
        return;
    }
    // Stride must come from the type, not a literal: `RectVertex` grew
    // when rounded corners landed, and a stale 24 here let 1024 vertices
    // be written into a buffer holding 512.
    let need = rects.len() * std::mem::size_of::<RectVertex>();
    if need > buffer.size() as usize {
        // 2x headroom, rounded up, so a busy overview does not
        // reallocate on every frame.
        *buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rects"),
            size: ((need * 2) as u64).next_multiple_of(4096),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
    }
    queue.write_buffer(buffer, 0, bytemuck::cast_slice(rects));
    pass.set_pipeline(pipeline);
    pass.set_vertex_buffer(0, buffer.slice(..));
    pass.draw(0..rects.len() as u32, 0..1);
}

/// One offscreen terminal render, sampled into an overview tile.
struct PreviewSlot {
    _tex: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
}

/// An offscreen frame to render this frame: which tab's target to draw into,
/// the rects for it, and where its single text buffer starts vertically.
pub struct PreviewJob {
    pub idx: usize,
    pub top: f32,
    pub bounds: glyphon::TextBounds,
    pub default_fg: [f32; 3],
    pub rects: Vec<RectVertex>,
}

/// A tile that samples a real rendered terminal frame. `half` is the box filter
/// half-width in UV: one destination pixel covers `1/tile_w` of the texture, so
/// a pixel averages exactly the source area it stands for.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PrevVertex {
    pos: [f32; 2],
    uv: [f32; 2],
    half: [f32; 2],
}

const PREV_VS_WGSL: &str = r#"
struct VSOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) half: vec2<f32>,
};
@vertex
fn vs(@location(0) pos: vec2<f32>,
      @location(1) uv: vec2<f32>,
      @location(2) half: vec2<f32>) -> VSOut {
    return VSOut(vec4<f32>(pos, 0.0, 1.0), uv, half);
}
"#;

const PREV_FS_WGSL: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct VSIn {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) half: vec2<f32>,
};
// 4x4 box filter. Bilinear alone samples one texel in eight at this ratio and
// aliases worse than the colour blocks it replaces, so every destination pixel
// averages the whole source region it covers.
@fragment
fn fs(i: VSIn) -> @location(0) vec4<f32> {
    var acc = vec4<f32>(0.0);
    for (var y: i32 = 0; y < 4; y = y + 1) {
        for (var x: i32 = 0; x < 4; x = x + 1) {
            let f = (vec2<f32>(f32(x), f32(y)) + vec2<f32>(0.5)) * vec2<f32>(0.25, 0.25) - vec2<f32>(0.5);
            acc = acc + textureSampleLevel(tex, samp, i.uv + f * i.half * vec2<f32>(2.0, 2.0), 0.0);
        }
    }
    return acc / 16.0;
}
"#;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
    /// True when the compositor will blend our alpha with whatever is behind the
    /// window, which is the only situation where a translucent terminal
    /// background is possible at all.
    pub alpha_ok: bool,
    pub rect_pipeline: wgpu::RenderPipeline,
    /// Rect pipeline for the tab-switcher overlay phase: premultiplied
    /// "src over" instead of the plain alpha blending (see init).
    pub overlay_rect_pipeline: wgpu::RenderPipeline,
    pub rect_buffer: wgpu::Buffer,
    /// Vertex storage for the switcher phase. Must not share `rect_buffer`
    /// with the base phase: both phases write at offset 0 of their buffer and
    /// both draws encode before the single frame submit, so a shared buffer
    /// would make the base draw read the switcher's vertices (the base
    /// background rect would be replaced by a double-drawn dim).
    pub overlay_rect_buffer: wgpu::Buffer,
    pub viewport: Viewport,
    pub atlas: TextAtlas,
    pub swash_cache: SwashCache,
    pub text_renderer: TextRenderer,
    /// Separate vertex storage: preparing overlay labels must not overwrite
    /// terminal glyphs whose draw is encoded but has not been submitted yet.
    overlay_text_renderer: TextRenderer,
    pub rects: Vec<RectVertex>,
    /// Pooled cosmic_text buffers for terminal runs (stable addresses).
    pub pool: Vec<Buffer>,
    /// Per-slot signature of what each pooled buffer currently holds. Lets an
    /// unchanged run skip shaping, which is the single most expensive thing a
    /// frame does (see `run_sig`).
    pub slot_sig: Vec<u64>,
    /// Pooled buffers for tab-strip labels.
    pub tab_pool: Vec<Buffer>,
    /// Pooled buffers for tab-switcher tile text. Kept separate from `pool`
    /// because the overview and the terminal draw in the same frame: sharing
    /// buffers would let the tile headers overwrite terminal runs at prepare
    /// time.
    pub ov_pool: Vec<Buffer>,
    /// Single buffer for the inline rename prompt (terminal font metrics).
    pub prompt_buf: Buffer,
    // --- Overview previews: real terminal frames, sampled down -------------
    /// One buffer per tab holding that tab's whole screen as one multi-line
    /// buffer. Its own pool, because `draw_overview` spends `pool` slots on tile
    /// headers in the same frame and they would overwrite each other.
    pub prev_bufs: Vec<Buffer>,
    /// Shaped content signature per preview buffer, so an unchanged screen is
    /// only re-drawn, never re-shaped.
    pub prev_sigs: Vec<u64>,
    /// When each preview was last rebuilt, for the per-tab throttle, and the
    /// rotating cursor that decides who goes next when there is a backlog.
    pub prev_last: Vec<Instant>,
    pub prev_rr: usize,
    prev_sampler: wgpu::Sampler,
    prev_bgl: wgpu::BindGroupLayout,
    prev_pipeline: wgpu::RenderPipeline,
    prev_buffer: wgpu::Buffer,
    /// Tile quads that sample a preview, six vertices each, and which tab each
    /// one reads. Filled by `push_preview_quad`, drawn between the rect pass and
    /// the text pass so tile headers stay on top.
    prev_quads: Vec<PrevVertex>,
    prev_quad_tab: Vec<usize>,
    /// Backing for `prev_viewport` (a glyphon Cache is a uniform buffer).
    prev_cache: Cache,
    previews: Vec<Option<PreviewSlot>>,
    /// Viewport over the offscreen targets, so the same `TextRenderer` can draw
    /// into them: same format, so no second atlas is needed.
    prev_viewport: Option<Viewport>,
    prev_size: (u32, u32),
}

pub(crate) fn block_on<F: std::future::Future>(fut: F) -> F::Output {
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

impl Gpu {
    pub fn init(
        event_loop: &winit::event_loop::ActiveEventLoop,
        window: &Arc<Window>,
        font_system: &mut FontSystem,
        font_px: f32,
        font_lh: f32,
        tab_font_px: f32,
        tab_font_lh: f32,
    ) -> Gpu {
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(
                event_loop.owned_display_handle(),
            )),
        );
        let surface = instance.create_surface(window.clone()).expect("surface");
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .expect("no adapter");
        log::info!("adapter: {}", adapter.get_info().name);
        // Pick a present mode the surface actually supports. Prefer Mailbox
        // (low latency), but some software/nested surfaces only offer Fifo.
        let caps = surface.get_capabilities(&adapter);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if let Some(&first) = caps.present_modes.first() {
            first
        } else {
            wgpu::PresentMode::Fifo
        };
        let (device, queue) =
            block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("no device");

        // Non-sRGB format: our colors are display-referred (sRGB) and are
        // written as-is. An sRGB target would re-encode them (double
        // conversion, washing colors out).
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let (width, height) =
            (window.inner_size().width.max(1), window.inner_size().height.max(1));
        // A translucent background needs the compositor to blend our alpha with
        // what is behind the window, so ask for premultiplied compositing when
        // the surface offers it. Nothing else has to change to produce it: both
        // the rect and the glyph pipelines blend with SRC_ALPHA/ONE_MINUS_SRC_ALPHA,
        // which writes a correctly premultiplied framebuffer, and the pass has
        // always cleared to TRANSPARENT -- the alpha channel was simply ignored
        // while the surface was Opaque.
        // Whether the compositor will honour our alpha at all. Do NOT gate this
        // on PreMultiplied alone: on Wayland the Vulkan swapchain typically
        // advertises only Opaque|Inherit, because INHERIT means the compositor
        // uses the buffer's own format semantics -- and an ARGB buffer's alpha is
        // honoured, premultiplied as wl_shm requires. Gating on PreMultiplied
        // silently disabled transparency on a compositor that supports it.
        let modes = &caps.alpha_modes;
        let want = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::Inherit,
        ];
        let alpha_ok = modes.iter().any(|m| want.contains(m));
        let alpha_mode = if modes.contains(&want[0]) {
            want[0]
        } else if modes.contains(&want[1]) {
            want[1]
        } else {
            wgpu::CompositeAlphaMode::Auto
        };
        log::info!(
            "alpha: surface offers {:?}, using {:?} (translucent background: {})",
            modes,
            alpha_mode,
            alpha_ok
        );
        if !alpha_ok {
            log::warn!(
                "surface offers no usable alpha ({:?}); terminal background stays opaque",
                modes
            );
        }
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        // Overview previews: a real offscreen terminal frame, sampled into a tile.
        let prev_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("preview"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let prev_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("preview"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let prev_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("preview"),
            bind_group_layouts: &[Some(&prev_bgl)],
            immediate_size: 0,
        });
        let prev_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("preview"),
            layout: Some(&prev_layout),
            vertex: wgpu::VertexState {
                module: &device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("preview.vs"),
                    source: wgpu::ShaderSource::Wgsl(PREV_VS_WGSL.into()),
                }),
                compilation_options: Default::default(),
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<PrevVertex>() as u64,
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
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        wgpu::VertexAttribute {
                            offset: 16,
                            shader_location: 2,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                    ],
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("preview.fs"),
                    source: wgpu::ShaderSource::Wgsl(PREV_FS_WGSL.into()),
                }),
                compilation_options: Default::default(),
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // Tiles are opaque by construction, so no blending: a
                    // translucent thumbnail is just mud over the backdrop.
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let prev_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview quads"),
            size: (64 * std::mem::size_of::<PrevVertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let prev_cache = Cache::new(&device);
        let cache = Cache::new(&device);
        // ColorMode::Web passes the text color through as-is. Accurate would
        // apply an sRGB->linear conversion in the shader, which would mismatch
        // our as-is non-sRGB surface and our display-referred sRGB colors.
        let mut atlas =
            TextAtlas::with_color_mode(&device, &queue, &cache, format, ColorMode::Web);
        let swash_cache = SwashCache::new();
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let overlay_text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let mut viewport = Viewport::new(&device, &cache);
        viewport.update(&queue, Resolution { width, height });

        // One layout/shader, two blend states. The base rects use the plain
        // SRC_ALPHA/ONE_MINUS_SRC_ALPHA blending the app was tuned against; the
        // overlay rects are pushed premultiplied and need "src over" in
        // premultiplied space (see below).
        let make_rect_pipeline = |label: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(
                    &device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor::default()),
                ),
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
                            wgpu::VertexAttribute {
                                offset: 24,
                                shader_location: 2,
                                format: wgpu::VertexFormat::Float32x2,
                            },
                            wgpu::VertexAttribute {
                                offset: 32,
                                shader_location: 3,
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
                        blend,
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
            })
        };
        let rect_pipeline = make_rect_pipeline("rects", Some(wgpu::BlendState::ALPHA_BLENDING));
        // "src over" alpha for the overlay layer. The surface is
        // premultiplied, and straight-colour rects over a premultiplied
        // target composite correctly with the usual
        // SRC_ALPHA/ONE_MINUS_SRC_ALPHA colour equation. The alpha channel
        // must follow the layer equation `src_a + (1 - src_a) * dst_a` --
        // i.e. factors (One, OneMinusSrcAlpha). Plain ALPHA_BLENDING would
        // compute `src_a * src_a + (1 - src_a) * dst_a`: the layer's own
        // alpha attenuated by itself, so the two transparencies (dim and
        // terminal background) combined multiplicatively and the window
        // came out *more* transparent than both -- exactly what the old
        // switcher did. "Src over" only ever adds opacity, like a real
        // translucent layer in a compositor.
        let overlay_rect_pipeline = make_rect_pipeline(
            "overlay rects",
            Some(wgpu::BlendState {
                color: wgpu::BlendComponent {
                    operation: wgpu::BlendOperation::Add,
                    src_factor: wgpu::BlendFactor::SrcAlpha,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                },
                alpha: wgpu::BlendComponent {
                    operation: wgpu::BlendOperation::Add,
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                },
            }),
        );
        let make_rect_buffer = |device: &wgpu::Device, label: &str| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (1024 * std::mem::size_of::<RectVertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let rect_buffer = make_rect_buffer(&device, "rects");
        let overlay_rect_buffer = make_rect_buffer(&device, "overlay rects");

        let pool: Vec<Buffer> = (0..MAX_RUNS)
            .map(|_| Buffer::new(font_system, Metrics::new(font_px, font_lh)))
            .collect();
        let tab_pool: Vec<Buffer> = (0..16)
            .map(|_| Buffer::new(font_system, Metrics::new(tab_font_px, tab_font_lh)))
            .collect();
        let prompt_buf = Buffer::new(font_system, Metrics::new(font_px, font_lh));
        let ov_pool: Vec<Buffer> = (0..64)
            .map(|_| Buffer::new(font_system, Metrics::new(tab_font_px, tab_font_lh)))
            .collect();

        Gpu {
            device,
            queue,
            surface,
            config,
            rect_pipeline,
            overlay_rect_pipeline,
            rect_buffer,
            overlay_rect_buffer,
            viewport,
            atlas,
            swash_cache,
            text_renderer,
            overlay_text_renderer,
            rects: Vec::with_capacity(1024),
            pool,
            slot_sig: vec![0u64; MAX_RUNS],
            alpha_ok,
            prev_bufs: Vec::new(),
            prev_sigs: Vec::new(),
            prev_last: Vec::new(),
            prev_rr: 0,
            prev_sampler,
            prev_bgl,
            prev_pipeline,
            prev_buffer,
            prev_quads: Vec::new(),
            prev_quad_tab: Vec::new(),
            prev_cache,
            previews: Vec::new(),
            prev_viewport: None,
            prev_size: (0, 0),
            tab_pool,
            ov_pool,
            prompt_buf,
        }
    }

    /// Upload and draw one batch of rect vertices.
    fn draw_rects(&mut self, pass: &mut wgpu::RenderPass<'_>, rects: &[RectVertex]) {
        let pipeline = self.rect_pipeline.clone();
        draw_rects(
            &self.device,
            &self.queue,
            pass,
            &mut self.rect_buffer,
            &pipeline,
            rects,
        );
    }

    fn draw_overlay_rects(&mut self, pass: &mut wgpu::RenderPass<'_>, rects: &[RectVertex]) {
        let pipeline = self.overlay_rect_pipeline.clone();
        draw_rects(
            &self.device,
            &self.queue,
            pass,
            &mut self.overlay_rect_buffer,
            &pipeline,
            rects,
        );
    }

    /// Render one frame: all rects, then all text areas.
    /// Clip-space vertices for one sampled tile, mirroring `rect_vertices`.
    pub fn push_preview_quad(
        &mut self,
        tab: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        wf: f32,
        hf: f32,
    ) {
        if self.previews.get(tab).map(|p| p.is_none()).unwrap_or(true) {
            return;
        }
        // Never drawn: the texture holds nothing yet, so sampling it would show
        // garbage. The tile keeps its empty well for that one frame.
        if self.prev_sigs.get(tab).copied().unwrap_or(0) == 0 {
            return;
        }
        let cx = |px: f32| px / wf * 2.0 - 1.0;
        // y points down in pixels, up in clip space.
        let cy = |py: f32| 1.0 - py / hf * 2.0;
        // One destination pixel covers 1/tile_w of the texture in UV.
        let half = [0.5 / w.max(1.0), 0.5 / h.max(1.0)];
        let v = |px: f32, py: f32, u: f32, vv: f32| PrevVertex {
            pos: [cx(px), cy(py)],
            uv: [u, vv],
            half,
        };
        let (x1, y1) = (x + w, y + h);
        self.prev_quads.extend_from_slice(&[
            v(x, y, 0.0, 0.0),
            v(x1, y, 1.0, 0.0),
            v(x1, y1, 1.0, 1.0),
            v(x, y, 0.0, 0.0),
            v(x1, y1, 1.0, 1.0),
            v(x, y1, 0.0, 1.0),
        ]);
        self.prev_quad_tab.push(tab);
    }

    pub fn set_prev_sig(&mut self, idx: usize, sig: u64) {
        if let Some(s) = self.prev_sigs.get_mut(idx) {
            *s = sig;
        }
    }

    /// Make room for `n` tabs rendering at `size`, and create tab `idx`'s
    /// texture. Textures are window-sized, so a tile can sample the whole window
    /// and the tab strip comes with it -- a picture of the window, not of a grid.
    pub fn ensure_previews(&mut self, n: usize, size: (u32, u32), fs: &mut FontSystem) {
        let (w, h) = size;
        if w == 0 || h == 0 {
            return;
        }
        if self.prev_size != size {
            self.previews.clear();
            self.prev_size = size;
            self.prev_viewport = Some(Viewport::new(&self.device, &self.prev_cache));
        }
        while self.prev_bufs.len() < n {
            self.prev_bufs
                .push(Buffer::new(fs, Metrics::new(1.0, 1.0)));
            self.prev_sigs.push(0);
            self.prev_last.push(Instant::now());
        }
        while self.previews.len() < n {
            let i = self.previews.len();
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("preview"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                dimension: wgpu::TextureDimension::D2,
                format: self.config.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                mip_level_count: 1,
                sample_count: 1,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("preview"),
                layout: &self.prev_bgl,                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.prev_sampler),
                    },
                ],
            });
            log::debug!("preview target {i}: {w}x{h}");
            self.previews.push(Some(PreviewSlot {
                _tex: tex,
                view,
                bind,
            }));
        }
    }

    pub fn render_frame(
        &mut self,
        font_system: &mut FontSystem,
        areas: Vec<glyphon::TextArea>,
        overlay: Option<OverlayPhase<'_>>,
        previews: Vec<PreviewJob>,
        window: &Window,
    ) {
        let (w, h) = (self.config.width, self.config.height);
        let size = self.prev_size;

        // Real terminal frames into offscreen targets, before the main pass. Each
        // job owns a buffer, so nothing here fights the main pass or the tiles for
        // pool slots. Shaping already happened in the caller; this only positions
        // and rasterises.
        if !previews.is_empty() {
            if let Some(vp) = self.prev_viewport.as_mut() {
                vp.update(&self.queue, Resolution {
                    width: size.0.max(1),
                    height: size.1.max(1),
                });
            }
        }
        for job in previews.iter() {
            let pview = match self.previews.get(job.idx) {
                Some(Some(slot)) => slot.view.clone(),
                _ => continue,
            };
            let buf = match self.prev_bufs.get(job.idx) {
                Some(b) => b,
                None => continue,
            };
            let mut penc =
                self.device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("preview"),
                    });
            {
                let mut ppass = penc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("preview"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &pview,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // Opaque: the window background rect carries the
                            // see-through alpha, and a translucent thumbnail is
                            // just mud. The tile quad is drawn without blending.
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                if !job.rects.is_empty() {
                    let need = job.rects.len() * std::mem::size_of::<RectVertex>();
                    if need > self.rect_buffer.size() as usize {
                        self.rect_buffer =
                            self.device.create_buffer(&wgpu::BufferDescriptor {
                                label: Some("rects"),
                                size: ((need * 2) as u64).next_multiple_of(4096),
                                usage: wgpu::BufferUsages::VERTEX
                                    | wgpu::BufferUsages::COPY_DST,
                                mapped_at_creation: false,
                            });
                    }
                    let pipeline = self.rect_pipeline.clone();
                    draw_rects(
                        &self.device,
                        &self.queue,
                        &mut ppass,
                        &mut self.rect_buffer,
                        &pipeline,
                        &job.rects,
                    );
                }
                if size.0 > 0 && size.1 > 0 {
                    let area = glyphon::TextArea {
                        buffer: buf,
                        left: 0.0,
                        top: job.top,
                        scale: 1.0,
                        bounds: job.bounds,
                        default_color: glyphon::Color::rgb(
                            (job.default_fg[0] * 255.0) as u8,
                            (job.default_fg[1] * 255.0) as u8,
                            (job.default_fg[2] * 255.0) as u8,
                        ),
                        custom_glyphs: &[],
                    };
                    if let Some(vp) = self.prev_viewport.as_mut() {
                        let prep = self.text_renderer.prepare(
                            &self.device,
                            &self.queue,
                            font_system,
                            &mut self.atlas,
                            vp,
                            [area],
                            &mut self.swash_cache,
                        );
                        if prep.is_ok() {
                            let _ = self.text_renderer.render(&self.atlas, vp, &mut ppass);
                        }
                    }
                }
            }
            self.queue.submit(Some(penc.finish()));
        }

        let frame_tex = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            other => {
                if matches!(other, wgpu::CurrentSurfaceTexture::Timeout) {
                    log::debug!("surface timeout, will retry");
                }
                window.request_redraw();
                return;
            },
        };
        let view = frame_tex.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
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

            // Phase 1: the base rects, then the live terminal text. The
            // pipelines are cloned out first: draw_rects mutably borrows
            // `self`, which the &self pipeline fields are still borrowed from.
            let rects = std::mem::take(&mut self.rects);
            self.draw_rects(&mut pass, &rects);
            if !areas.is_empty() {
                self.text_renderer
                    .prepare(
                        &self.device,
                        &self.queue,
                        font_system,
                        &mut self.atlas,
                        &self.viewport,
                        areas,
                        &mut self.swash_cache,
                    )
                    .expect("prepare");
                self.text_renderer
                    .render(&self.atlas, &self.viewport, &mut pass)
                    .expect("render");
            }

            // Phase 2 (switcher open): the dim and the tiles above the
            // terminal text, the sampled thumbnails above the tiles, and the
            // tile text on top.
            if let Some(overlay) = overlay {
                self.draw_overlay_rects(&mut pass, &overlay.rects);

                // Sampled tiles: after the rects, so each tile's own background
                // is underneath, and before the text, so headers stay on top.
                let quads = std::mem::take(&mut self.prev_quads);
                let owners = std::mem::take(&mut self.prev_quad_tab);
                if !quads.is_empty() {
                    let need = quads.len() * std::mem::size_of::<PrevVertex>();
                    if need > self.prev_buffer.size() as usize {
                        self.prev_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("preview quads"),
                            size: ((need * 2) as u64).next_multiple_of(4096),
                            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        });
                    }
                    self.queue
                        .write_buffer(&self.prev_buffer, 0, bytemuck::cast_slice(&quads));
                    pass.set_pipeline(&self.prev_pipeline);
                    pass.set_vertex_buffer(0, self.prev_buffer.slice(..));
                    for (n, (chunk, tab)) in quads.chunks(6).zip(owners.iter()).enumerate() {
                        if let Some(Some(slot)) = self.previews.get(*tab) {
                            // Slice per quad: binding the whole buffer and drawing
                            // 0..6 every time would paint the first tile's geometry
                            // once per texture, so only one tile showed anything and
                            // it showed the wrong tab.
                            let v = std::mem::size_of::<PrevVertex>() as u64;
                            let first = (n * 6) as u64 * v;
                            pass.set_vertex_buffer(0, self.prev_buffer.slice(first..first + 6 * v));
                            pass.set_bind_group(0, &slot.bind, &[]);
                            pass.draw(0..chunk.len() as u32, 0..1);
                        }
                    }
                }

                if !overlay.areas.is_empty() {
                    self.overlay_text_renderer
                        .prepare(
                            &self.device,
                            &self.queue,
                            font_system,
                            &mut self.atlas,
                            &self.viewport,
                            overlay.areas,
                            &mut self.swash_cache,
                        )
                        .expect("prepare");
                    self.overlay_text_renderer
                        .render(&self.atlas, &self.viewport, &mut pass)
                        .expect("render");
                }
            }
        }
        // Wayland uses this to request the compositor's next frame callback.
        // Without it a busy PTY can drive redraws as fast as the CPU can submit
        // them, even when those frames can never be displayed.
        window.pre_present_notify();
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame_tex);
        let _ = (w, h);
    }

}
