//! GPU state: wgpu device/surface, rect pipeline (backgrounds, cursor,
//! tab strip), glyphon text rendering.

use std::sync::Arc;

use glyphon::{Buffer, Cache, ColorMode, FontSystem, Metrics, Resolution, SwashCache, TextAtlas, TextRenderer, Viewport};
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

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
    pub rect_pipeline: wgpu::RenderPipeline,
    pub rect_buffer: wgpu::Buffer,
    pub viewport: Viewport,
    pub atlas: TextAtlas,
    pub swash_cache: SwashCache,
    pub text_renderer: TextRenderer,
    pub rects: Vec<RectVertex>,
    /// Pooled cosmic_text buffers for terminal runs (stable addresses).
    pub pool: Vec<Buffer>,
    /// Pooled buffers for tab-strip labels.
    pub tab_pool: Vec<Buffer>,
    /// Single buffer for the inline rename prompt (terminal font metrics).
    pub prompt_buf: Buffer,
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
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let cache = Cache::new(&device);
        // ColorMode::Web passes the text color through as-is. Accurate would
        // apply an sRGB->linear conversion in the shader, which would mismatch
        // our as-is non-sRGB surface and our display-referred sRGB colors.
        let mut atlas =
            TextAtlas::with_color_mode(&device, &queue, &cache, format, ColorMode::Web);
        let swash_cache = SwashCache::new();
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let mut viewport = Viewport::new(&device, &cache);
        viewport.update(&queue, Resolution { width, height });

        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rects"),
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
            size: (1024 * std::mem::size_of::<RectVertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let pool: Vec<Buffer> = (0..MAX_RUNS)
            .map(|_| Buffer::new(font_system, Metrics::new(font_px, font_lh)))
            .collect();
        let tab_pool: Vec<Buffer> = (0..16)
            .map(|_| Buffer::new(font_system, Metrics::new(tab_font_px, tab_font_lh)))
            .collect();
        let prompt_buf = Buffer::new(font_system, Metrics::new(font_px, font_lh));

        Gpu {
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
            tab_pool,
            prompt_buf,
        }
    }

    /// Render one frame: all rects, then all text areas.
    pub fn render_frame(
        &mut self,
        font_system: &mut FontSystem,
        areas: Vec<glyphon::TextArea>,
        window: &Window,
    ) {
        let (w, h) = (self.config.width, self.config.height);

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

            pass.set_pipeline(&self.rect_pipeline);
            // Stride must come from the type, not a literal: `RectVertex` grew
            // when rounded corners landed, and a stale 24 here let 1024 vertices
            // be written into a buffer holding 512.
            let need = self.rects.len() * std::mem::size_of::<RectVertex>();
            if need > self.rect_buffer.size() as usize {
                self.rect_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("rects"),
                    // 2x headroom, rounded up, so a busy overview does not
                    // reallocate on every frame.
                    size: ((need * 2) as u64).next_multiple_of(4096),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            self.queue
                .write_buffer(&self.rect_buffer, 0, bytemuck::cast_slice(&self.rects));
            pass.set_vertex_buffer(0, self.rect_buffer.slice(..));
            pass.draw(0..self.rects.len() as u32, 0..1);

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
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame_tex);
        let _ = (w, h);
    }
}
