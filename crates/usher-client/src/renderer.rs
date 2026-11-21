use crate::decoder::Yuv420Frame;
use crate::overlay::OverlayRenderOutput;
use anyhow::{Context, Result, anyhow};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, RawWaker, RawWakerVTable, Waker};
use wgpu::util::DeviceExt;
use winit::window::Window;

const SHADER_WGSL: &str = r#"
struct Uniforms {
    scale: vec2<f32>,
    is_srgb: f32,
    _pad: f32,
};

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var u_tex: texture_2d<f32>;
@group(0) @binding(2) var v_tex: texture_2d<f32>;
@group(0) @binding(3) var video_sampler: sampler;
@group(0) @binding(4) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    // Fullscreen triangle covering clip space [-1, 1] x [-1, 1]
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let p = positions[idx];
    let ndc = p / u.scale;
    var out: VsOut;
    out.pos = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let low = c / 12.92;
    let high = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    // BT.709 limited-range YUV420P -> sRGB
    let y_val = (textureSample(y_tex, video_sampler, in.uv).r - (16.0 / 255.0)) * (255.0 / 219.0);
    let u_val = (textureSample(u_tex, video_sampler, in.uv).r - 0.5) * (255.0 / 224.0);
    let v_val = (textureSample(v_tex, video_sampler, in.uv).r - 0.5) * (255.0 / 224.0);

    let r = y_val + 1.5748 * v_val;
    let g = y_val - 0.1873 * u_val - 0.4681 * v_val;
    let b = y_val + 1.8556 * u_val;
    let srgb = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));

    let out_rgb = select(srgb, srgb_to_linear(srgb), u.is_srgb > 0.5);
    return vec4<f32>(out_rgb, 1.0);
}
"#;

const OVERLAY_SHADER_WGSL: &str = r#"
struct OverlayRect {
    rect: vec4<f32>,
};

@group(0) @binding(0) var ov_tex: texture_2d<f32>;
@group(0) @binding(1) var ov_sampler: sampler;
@group(0) @binding(2) var<uniform> ov: OverlayRect;

struct OvOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn ov_vs(@builtin(vertex_index) idx: u32) -> OvOut {
    var pos = array<vec2<f32>, 6>(
        vec2<f32>(ov.rect.x, ov.rect.w),
        vec2<f32>(ov.rect.z, ov.rect.w),
        vec2<f32>(ov.rect.x, ov.rect.y),
        vec2<f32>(ov.rect.x, ov.rect.y),
        vec2<f32>(ov.rect.z, ov.rect.w),
        vec2<f32>(ov.rect.z, ov.rect.y),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );
    var out: OvOut;
    out.pos = vec4<f32>(pos[idx], 0.0, 1.0);
    out.uv = uvs[idx];
    return out;
}

@fragment
fn ov_fs(in: OvOut) -> @location(0) vec4<f32> {
    return textureSample(ov_tex, ov_sampler, in.uv);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct OverlayRectUniform {
    x0: f32,
    y_bot: f32,
    x1: f32,
    y_top: f32,
}

impl OverlayRectUniform {
    fn as_bytes(&self) -> &[u8] {
        let ptr = (self as *const Self).cast::<u8>();
        unsafe { std::slice::from_raw_parts(ptr, std::mem::size_of::<Self>()) }
    }
}

struct QuadResources {
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buf: wgpu::Buffer,
    tex_w: u32,
    tex_h: u32,
    tex: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    format: wgpu::TextureFormat,
    visible: bool,
}

struct OverlayResources {
    quad: QuadResources,
    last_panel: (u32, u32, u32, u32),
}

pub struct CursorRenderData<'a> {
    pub shape_id: u32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub norm_x: f32,
    pub norm_y: f32,
    pub rgba: &'a [u8],
}

struct CursorResources {
    quad: QuadResources,
    shape_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UniformData {
    scale_x: f32,
    scale_y: f32,
    is_srgb: f32,
    pad: f32,
}

impl UniformData {
    fn as_bytes(&self) -> &[u8] {
        let ptr = (self as *const Self).cast::<u8>();
        unsafe { std::slice::from_raw_parts(ptr, std::mem::size_of::<Self>()) }
    }
}

struct YuvTextures {
    width: u32,
    height: u32,
    is_444: bool,
    y_tex: wgpu::Texture,
    u_tex: wgpu::Texture,
    v_tex: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buf: wgpu::Buffer,
    textures: YuvTextures,
    overlay: OverlayResources,
    cursor: CursorResources,
    has_frame: bool,
}

impl Renderer {
    pub fn new(window: Arc<Window>, initial_video_w: u32, initial_video_h: u32) -> Result<Self> {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window)
            .context("creating wgpu surface")?;

        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .map_err(|e| anyhow!("no compatible GPU adapter found: {e}"))?;

        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("usher-device"),
            ..Default::default()
        }))
        .map_err(|e| anyhow!("request_device failed: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv420p-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_WGSL.into()),
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("yuv-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let initial_uniform = compute_uniform(
            config.width,
            config.height,
            initial_video_w,
            initial_video_h,
            format.is_srgb(),
        );
        let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("aspect-uniform"),
            contents: initial_uniform.as_bytes(),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv-bgl"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let textures = allocate_yuv_textures(
            &device,
            &bind_group_layout,
            &sampler,
            &uniform_buf,
            initial_video_w.max(2),
            initial_video_h.max(2),
            false,
        );

        let overlay = create_overlay_resources(&device, format);
        let cursor = create_cursor_resources(&device, format);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group_layout,
            sampler,
            uniform_buf,
            textures,
            overlay,
            cursor,
            has_frame: false,
        })
    }

    pub fn video_dimensions(&self) -> (u32, u32) {
        (self.textures.width, self.textures.height)
    }

    pub fn window_dimensions(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn update_overlay(&mut self, output: Option<&OverlayRenderOutput>) {
        let Some(out) = output else {
            self.overlay.quad.visible = false;
            return;
        };
        if out.width == 0 || out.height == 0 || out.rgba.len() < (out.width * out.height * 4) as usize {
            self.overlay.quad.visible = false;
            return;
        }

        if self.overlay.quad.tex_w != out.width || self.overlay.quad.tex_h != out.height {
            let (tex, bind_group) = allocate_quad_texture(
                &self.device,
                &self.overlay.quad.bgl,
                &self.overlay.quad.sampler,
                &self.overlay.quad.uniform_buf,
                "usher-overlay",
                self.overlay.quad.format,
                out.width,
                out.height,
            );
            self.overlay.quad.tex = tex;
            self.overlay.quad.bind_group = bind_group;
            self.overlay.quad.tex_w = out.width;
            self.overlay.quad.tex_h = out.height;
        }

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.overlay.quad.tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &out.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(out.width * 4),
                rows_per_image: Some(out.height),
            },
            wgpu::Extent3d {
                width: out.width,
                height: out.height,
                depth_or_array_layers: 1,
            },
        );

        self.overlay.last_panel = (out.panel_x, out.panel_y, out.panel_w, out.panel_h);
        self.write_overlay_rect_uniform();
        self.overlay.quad.visible = true;
    }

    pub fn update_cursor(&mut self, data: Option<CursorRenderData<'_>>) {
        let Some(c) = data else {
            self.cursor.quad.visible = false;
            return;
        };
        let cw = c.width as u32;
        let ch = c.height as u32;
        let expected_len = (cw * ch * 4) as usize;
        if cw == 0 || ch == 0 || c.rgba.len() < expected_len {
            self.cursor.quad.visible = false;
            return;
        }

        if self.cursor.quad.tex_w != cw || self.cursor.quad.tex_h != ch {
            let (tex, bind_group) = allocate_quad_texture(
                &self.device,
                &self.cursor.quad.bgl,
                &self.cursor.quad.sampler,
                &self.cursor.quad.uniform_buf,
                "usher-cursor",
                self.cursor.quad.format,
                cw,
                ch,
            );
            self.cursor.quad.tex = tex;
            self.cursor.quad.bind_group = bind_group;
            self.cursor.quad.tex_w = cw;
            self.cursor.quad.tex_h = ch;
            self.cursor.shape_id = 0;
        }

        if self.cursor.shape_id != c.shape_id {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.cursor.quad.tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &c.rgba[..expected_len],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(cw * 4),
                    rows_per_image: Some(ch),
                },
                wgpu::Extent3d {
                    width: cw,
                    height: ch,
                    depth_or_array_layers: 1,
                },
            );
            self.cursor.shape_id = c.shape_id;
        }

        let ww = self.config.width.max(1) as f64;
        let wh = self.config.height.max(1) as f64;
        let (rx, ry, rw, rh) = video_letterbox_rect(
            self.config.width,
            self.config.height,
            self.textures.width,
            self.textures.height,
        );
        let scale = (rw / self.textures.width.max(1) as f64)
            .min(rh / self.textures.height.max(1) as f64);
        let anchor_x = rx + (c.norm_x.clamp(0.0, 1.0) as f64) * rw;
        let anchor_y = ry + (c.norm_y.clamp(0.0, 1.0) as f64) * rh;
        let px = anchor_x - (c.hotspot_x as f64) * scale;
        let py = anchor_y - (c.hotspot_y as f64) * scale;
        let pw = (cw as f64) * scale;
        let ph = (ch as f64) * scale;
        let u = OverlayRectUniform {
            x0: ((px / ww) * 2.0 - 1.0) as f32,
            x1: (((px + pw) / ww) * 2.0 - 1.0) as f32,
            y_top: (1.0 - (py / wh) * 2.0) as f32,
            y_bot: (1.0 - ((py + ph) / wh) * 2.0) as f32,
        };
        self.queue
            .write_buffer(&self.cursor.quad.uniform_buf, 0, u.as_bytes());
        self.cursor.quad.visible = true;
    }

    fn write_overlay_rect_uniform(&self) {
        let (px, py, pw, ph) = self.overlay.last_panel;
        let ww = self.config.width.max(1) as f32;
        let wh = self.config.height.max(1) as f32;
        let u = OverlayRectUniform {
            x0: (px as f32 / ww) * 2.0 - 1.0,
            x1: ((px + pw) as f32 / ww) * 2.0 - 1.0,
            y_top: 1.0 - (py as f32 / wh) * 2.0,
            y_bot: 1.0 - ((py + ph) as f32 / wh) * 2.0,
        };
        self.queue
            .write_buffer(&self.overlay.quad.uniform_buf, 0, u.as_bytes());
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.update_uniform();
        if self.overlay.quad.visible {
            self.write_overlay_rect_uniform();
        }
    }

    pub fn render(&mut self, frame: Option<&Yuv420Frame>) {
        if let Some(f) = frame {
            if f.width != self.textures.width
                || f.height != self.textures.height
                || f.is_444 != self.textures.is_444
            {
                self.textures = allocate_yuv_textures(
                    &self.device,
                    &self.bind_group_layout,
                    &self.sampler,
                    &self.uniform_buf,
                    f.width.max(2),
                    f.height.max(2),
                    f.is_444,
                );
                self.update_uniform();
            }

            let (uv_w, uv_h) = if f.is_444 {
                (f.width, f.height)
            } else {
                (f.width / 2, f.height / 2)
            };
            upload_plane(&self.queue, &self.textures.y_tex, f.width, f.height, f.y_stride, &f.y);
            upload_plane(
                &self.queue,
                &self.textures.u_tex,
                uv_w,
                uv_h,
                f.u_stride,
                &f.u,
            );
            upload_plane(
                &self.queue,
                &self.textures.v_tex,
                uv_w,
                uv_h,
                f.v_stride,
                &f.v,
            );
            self.has_frame = true;
        }

        let (surface_tex, suboptimal) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(tex) => (tex, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(tex) => (tex, true),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return,
        };

        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("usher-render-encoder"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("usher-yuv-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if self.has_frame {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.textures.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            if self.cursor.quad.visible {
                pass.set_pipeline(&self.cursor.quad.pipeline);
                pass.set_bind_group(0, &self.cursor.quad.bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
            if self.overlay.quad.visible {
                pass.set_pipeline(&self.overlay.quad.pipeline);
                pass.set_bind_group(0, &self.overlay.quad.bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
        }

        self.queue.submit(Some(encoder.finish()));
        self.queue.present(surface_tex);

        if suboptimal {
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn update_uniform(&self) {
        let u = compute_uniform(
            self.config.width,
            self.config.height,
            self.textures.width,
            self.textures.height,
            self.config.format.is_srgb(),
        );
        self.queue.write_buffer(&self.uniform_buf, 0, u.as_bytes());
    }
}

/// Computes `(offset_x, offset_y, rect_w, rect_h)` of the letterboxed video inside
/// the window surface in physical pixels, used to map absolute cursor positions cleanly.
pub fn video_letterbox_rect(win_w: u32, win_h: u32, vid_w: u32, vid_h: u32) -> (f64, f64, f64, f64) {
    let ww = win_w.max(1) as f64;
    let wh = win_h.max(1) as f64;
    let vw = vid_w.max(1) as f64;
    let vh = vid_h.max(1) as f64;
    let win_aspect = ww / wh;
    let vid_aspect = vw / vh;
    if win_aspect > vid_aspect {
        let rw = wh * vid_aspect;
        let ox = (ww - rw) * 0.5;
        (ox, 0.0, rw, wh)
    } else {
        let rh = ww / vid_aspect;
        let oy = (wh - rh) * 0.5;
        (0.0, oy, ww, rh)
    }
}

fn compute_uniform(win_w: u32, win_h: u32, vid_w: u32, vid_h: u32, is_srgb: bool) -> UniformData {
    let win_aspect = win_w.max(1) as f32 / win_h.max(1) as f32;
    let vid_aspect = vid_w.max(1) as f32 / vid_h.max(1) as f32;
    let (scale_x, scale_y) = if win_aspect > vid_aspect {
        (vid_aspect / win_aspect, 1.0)
    } else {
        (1.0, win_aspect / vid_aspect)
    };
    UniformData {
        scale_x,
        scale_y,
        is_srgb: if is_srgb { 1.0 } else { 0.0 },
        pad: 0.0,
    }
}

fn tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn allocate_yuv_textures(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    uniform_buf: &wgpu::Buffer,
    width: u32,
    height: u32,
    is_444: bool,
) -> YuvTextures {
    let (uv_w, uv_h) = if is_444 {
        (width, height)
    } else {
        ((width / 2).max(1), (height / 2).max(1))
    };
    let y_tex = create_r8_texture(device, "y-plane", width, height);
    let u_tex = create_r8_texture(device, "u-plane", uv_w, uv_h);
    let v_tex = create_r8_texture(device, "v-plane", uv_w, uv_h);

    let y_view = y_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let u_view = u_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let v_view = v_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("yuv-bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&y_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&u_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&v_view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: uniform_buf.as_entire_binding(),
            },
        ],
    });

    YuvTextures {
        width,
        height,
        is_444,
        y_tex,
        u_tex,
        v_tex,
        bind_group,
    }
}

fn create_r8_texture(device: &wgpu::Device, label: &str, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn upload_plane(
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    width: u32,
    height: u32,
    stride: u32,
    data: &[u8],
) {
    if width == 0 || height == 0 || data.is_empty() {
        return;
    }
    let effective_stride = stride.max(width);
    let min_needed = (effective_stride as usize)
        .saturating_mul((height as usize).saturating_sub(1))
        .saturating_add(width as usize);
    if data.len() < min_needed {
        return;
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(effective_stride),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

fn block_on<F: Future>(fut: F) -> F::Output {
    let thread = std::thread::current();
    let ptr = Arc::into_raw(Arc::new(thread)).cast::<()>();
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |data| {
            let arc = unsafe { Arc::from_raw(data.cast::<std::thread::Thread>()) };
            let cloned = arc.clone();
            std::mem::forget(arc);
            RawWaker::new(Arc::into_raw(cloned).cast::<()>(), &VTABLE)
        },
        |data| {
            let arc = unsafe { Arc::from_raw(data.cast::<std::thread::Thread>()) };
            arc.unpark();
        },
        |data| {
            let arc = unsafe { Arc::from_raw(data.cast::<std::thread::Thread>()) };
            arc.unpark();
            std::mem::forget(arc);
        },
        |data| unsafe {
            drop(Arc::from_raw(data.cast::<std::thread::Thread>()));
        },
    );

    let waker = unsafe { Waker::from_raw(RawWaker::new(ptr, &VTABLE)) };
    let mut cx = TaskContext::from_waker(&waker);
    let mut fut = Pin::from(Box::new(fut));
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn create_quad_resources(
    device: &wgpu::Device,
    surface_format: wgpu::TextureFormat,
    label_prefix: &str,
) -> QuadResources {
    let tex_format = if surface_format.is_srgb() {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };

    let shader_label = format!("{label_prefix}-shader");
    let sampler_label = format!("{label_prefix}-sampler");
    let uniform_label = format!("{label_prefix}-rect-uniform");
    let bgl_label = format!("{label_prefix}-bgl");
    let layout_label = format!("{label_prefix}-layout");
    let pipeline_label = format!("{label_prefix}-pipeline");

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(&shader_label),
        source: wgpu::ShaderSource::Wgsl(OVERLAY_SHADER_WGSL.into()),
    });

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(&sampler_label),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    let init_uniform = OverlayRectUniform {
        x0: -0.5,
        y_bot: -0.5,
        x1: 0.5,
        y_top: 0.5,
    };
    let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(&uniform_label),
        contents: init_uniform.as_bytes(),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });

    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(&bgl_label),
        entries: &[
            tex_entry(0),
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });

    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(&layout_label),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(&pipeline_label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("ov_vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("ov_fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    });

    let (tex, bind_group) = allocate_quad_texture(
        device,
        &bgl,
        &sampler,
        &uniform_buf,
        label_prefix,
        tex_format,
        4,
        4,
    );

    QuadResources {
        pipeline,
        bgl,
        sampler,
        uniform_buf,
        tex_w: 4,
        tex_h: 4,
        tex,
        bind_group,
        format: tex_format,
        visible: false,
    }
}

fn create_overlay_resources(
    device: &wgpu::Device,
    surface_format: wgpu::TextureFormat,
) -> OverlayResources {
    OverlayResources {
        quad: create_quad_resources(device, surface_format, "usher-overlay"),
        last_panel: (0, 0, 4, 4),
    }
}

fn create_cursor_resources(
    device: &wgpu::Device,
    surface_format: wgpu::TextureFormat,
) -> CursorResources {
    CursorResources {
        quad: create_quad_resources(device, surface_format, "usher-cursor"),
        shape_id: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn allocate_quad_texture(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    uniform_buf: &wgpu::Buffer,
    label_prefix: &str,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::BindGroup) {
    let tex_label = format!("{label_prefix}-tex");
    let bg_label = format!("{label_prefix}-bg");
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(&tex_label),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(&bg_label),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: uniform_buf.as_entire_binding(),
            },
        ],
    });
    (tex, bind_group)
}
