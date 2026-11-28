//! GPU renderer: uploads I420 frames to textures and draws them into any
//! wgpu target with [`fit_viewport`], so every aspect ratio shows centered and
//! undistorted on black.
//!
//! The Y, U and V planes are separate `R8Unorm` textures; the fragment shader
//! converts BT.601 limited range to RGB. The renderer draws into a caller's
//! `TextureView` (a window surface or an offscreen texture), so it does not
//! depend on a windowing library.

mod viewport;

pub use viewport::{fit_viewport, Viewport};

use thiserror::Error;

use crate::frame::YuvFrame;
use crate::scale::Resolution;

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("no GPU adapter available: {0}")]
    NoAdapter(String),
    #[error("cannot create the GPU device: {0}")]
    Device(String),
    #[error("reading the rendered image back failed: {0}")]
    Readback(String),
}

const SHADER: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle that covers the viewport; uv runs 0..1 over the frame.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var o: Out;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_u: texture_2d<f32>;
@group(0) @binding(2) var tex_v: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    // BT.601, limited range (Y 16..235, UV 16..240).
    let y = (textureSample(tex_y, samp, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (textureSample(tex_u, samp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (textureSample(tex_v, samp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let rgb = vec3<f32>(
        y + 1.402 * v,
        y - 0.344136 * u - 0.714136 * v,
        y + 1.772 * u,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

struct Planes {
    size: Resolution,
    textures: [wgpu::Texture; 3],
    bind_group: wgpu::BindGroup,
}

/// Draws [`YuvFrame`]s. Create one per target format.
///
/// Targets must use a non-sRGB format (for example `Bgra8Unorm` or
/// `Rgba8Unorm`, not `Bgra8UnormSrgb`). The shader writes gamma-encoded RGB
/// straight from BT.709 YUV; an sRGB target would encode it a second time and
/// the picture would come out washed out. For a surface, pick a non-sRGB entry
/// of the surface capabilities.
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    planes: Option<Planes>,
    adapter_info: Option<wgpu::AdapterInfo>,
}

impl Renderer {
    /// Uses an existing device, drawing into targets of `format`, which must
    /// not be an sRGB format (see [`Renderer`]).
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("v6cast yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("v6cast yuv planes"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("v6cast yuv"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("v6cast yuv"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("v6cast yuv"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            device,
            queue,
            pipeline,
            layout,
            sampler,
            planes: None,
            adapter_info: None,
        }
    }

    /// Creates its own device on any adapter, software ones included.
    /// wgpu's environment variables (`WGPU_BACKEND`, `WGPU_ADAPTER_NAME`) apply.
    pub fn headless(format: wgpu::TextureFormat) -> Result<Self, RenderError> {
        pollster::block_on(async {
            // Honours WGPU_BACKEND, WGPU_ADAPTER_NAME etc., e.g. to force a software adapter.
            let instance = wgpu::Instance::new(
                wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
            );
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    ..Default::default()
                })
                .await
                .map_err(|e| RenderError::NoAdapter(e.to_string()))?;
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor::default())
                .await
                .map_err(|e| RenderError::Device(e.to_string()))?;
            let mut renderer = Self::new(device, queue, format);
            renderer.adapter_info = Some(adapter.get_info());
            Ok(renderer)
        })
    }

    /// Name and backend of the adapter behind a headless renderer, if known.
    pub fn adapter_info(&self) -> Option<&wgpu::AdapterInfo> {
        self.adapter_info.as_ref()
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    fn planes_for(&mut self, size: Resolution) -> &Planes {
        if self.planes.as_ref().is_none_or(|p| p.size != size) {
            let plane = |w: u32, h: u32, name| {
                self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(name),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let (w, h) = (size.width, size.height);
            let textures = [
                plane(w, h, "v6cast y"),
                plane(w / 2, h / 2, "v6cast u"),
                plane(w / 2, h / 2, "v6cast v"),
            ];
            let views: Vec<wgpu::TextureView> = textures
                .iter()
                .map(|t| t.create_view(&Default::default()))
                .collect();
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("v6cast yuv planes"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&views[0]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&views[1]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&views[2]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.planes = Some(Planes {
                size,
                textures,
                bind_group,
            });
        }
        // Set just above.
        self.planes.as_ref().expect("planes exist")
    }

    /// Uploads `frame` to the GPU. Textures are reused while the size stays.
    pub fn upload(&mut self, frame: &YuvFrame) {
        let size = frame.resolution();
        let queue = self.queue.clone();
        let planes = self.planes_for(size);
        let (w, h) = (size.width, size.height);
        for (texture, (data, pw, ph)) in planes.textures.iter().zip([
            (frame.y(), w, h),
            (frame.u(), w / 2, h / 2),
            (frame.v(), w / 2, h / 2),
        ]) {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(pw),
                    rows_per_image: Some(ph),
                },
                wgpu::Extent3d {
                    width: pw,
                    height: ph,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    /// Clears `target` to black and draws the last uploaded frame centered
    /// with its aspect ratio kept. Draws only black before the first upload.
    pub fn render(&self, target: &wgpu::TextureView, target_size: Resolution) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("v6cast render"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("v6cast render"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let Some(planes) = &self.planes {
                if let Some(v) = fit_viewport(planes.size, target_size) {
                    pass.set_viewport(
                        v.x as f32,
                        v.y as f32,
                        v.width as f32,
                        v.height as f32,
                        0.0,
                        1.0,
                    );
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &planes.bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
        }
        self.queue.submit([encoder.finish()]);
    }

    /// Renders into a new offscreen RGBA texture of `size` and reads it back
    /// as tightly packed RGBA rows. Meant for tests and screenshots.
    pub fn render_to_rgba(&self, size: Resolution) -> Result<Vec<u8>, RenderError> {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("v6cast offscreen"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.render(&texture.create_view(&Default::default()), size);
        let row = size.width * 4;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("v6cast readback"),
            size: u64::from(padded) * u64::from(size.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(size.height),
                },
            },
            wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        rx.recv()
            .map_err(|e| RenderError::Readback(e.to_string()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let mapped = slice
            .get_mapped_range()
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let mut out = Vec::with_capacity((row * size.height) as usize);
        for y in 0..size.height as usize {
            out.extend_from_slice(&mapped[y * padded as usize..][..row as usize]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu() -> Option<Renderer> {
        match Renderer::headless(wgpu::TextureFormat::Rgba8Unorm) {
            Ok(r) => {
                if let Some(info) = r.adapter_info() {
                    eprintln!("renderer adapter: {} ({:?})", info.name, info.backend);
                }
                Some(r)
            }
            Err(e) => {
                eprintln!("skipping: {e}");
                None
            }
        }
    }

    /// A frame of one color (BT.601 limited-range YUV).
    fn solid(size: Resolution, y: u8, u: u8, v: u8) -> YuvFrame {
        let mut f = YuvFrame::black(size).unwrap();
        let (py, pu, pv) = f.planes_mut();
        py.fill(y);
        pu.fill(u);
        pv.fill(v);
        f
    }

    fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    fn near(a: [u8; 4], b: [u8; 3]) -> bool {
        a[..3].iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 6)
    }

    #[test]
    fn draws_black_before_the_first_frame() {
        let Some(renderer) = gpu() else { return };
        let rgba = renderer.render_to_rgba(Resolution::new(8, 8)).unwrap();
        assert!(rgba.chunks(4).all(|p| p == [0, 0, 0, 255]));
    }

    #[test]
    fn converts_bt601_colors() {
        let Some(mut renderer) = gpu() else { return };
        // (Y, U, V) -> expected RGB.
        let cases = [
            ((235, 128, 128), [255, 255, 255]),
            ((16, 128, 128), [0, 0, 0]),
            ((81, 90, 240), [255, 0, 0]),
            ((145, 54, 34), [0, 255, 0]),
            ((41, 240, 110), [0, 0, 255]),
        ];
        for ((y, u, v), rgb) in cases {
            renderer.upload(&solid(Resolution::new(16, 16), y, u, v));
            let rgba = renderer.render_to_rgba(Resolution::new(16, 16)).unwrap();
            let p = pixel(&rgba, 16, 8, 8);
            assert!(near(p, rgb), "YUV ({y},{u},{v}) -> {p:?}, expected {rgb:?}");
        }
    }

    #[test]
    fn ultrawide_frame_is_letterboxed_in_a_square_target() {
        let Some(mut renderer) = gpu() else { return };
        // White 4:1 frame in a 64x64 target: a 64x16 band in the middle.
        renderer.upload(&solid(Resolution::new(64, 16), 235, 128, 128));
        let rgba = renderer.render_to_rgba(Resolution::new(64, 64)).unwrap();
        let v = fit_viewport(Resolution::new(64, 16), Resolution::new(64, 64)).unwrap();
        assert_eq!((v.y, v.height), (24, 16));
        for (x, y, white) in [
            (32, 5, false),
            (32, 23, false),
            (32, 24, true),
            (2, 32, true),
            (32, 39, true),
            (32, 40, false),
            (32, 60, false),
        ] {
            let p = pixel(&rgba, 64, x, y);
            let expected = if white { [255, 255, 255] } else { [0, 0, 0] };
            assert!(near(p, expected), "({x},{y}) = {p:?}");
        }
    }

    #[test]
    fn tall_frame_is_pillarboxed_and_textures_follow_size_changes() {
        let Some(mut renderer) = gpu() else { return };
        renderer.upload(&solid(Resolution::new(64, 16), 235, 128, 128));
        // Size change: textures are recreated.
        renderer.upload(&solid(Resolution::new(16, 32), 81, 90, 240));
        let rgba = renderer.render_to_rgba(Resolution::new(64, 32)).unwrap();
        // 16x32 in 64x32: a red 16x32 column at x = 24..40.
        for (x, red) in [
            (5, false),
            (23, false),
            (24, true),
            (32, true),
            (39, true),
            (40, false),
            (60, false),
        ] {
            let p = pixel(&rgba, 64, x, 16);
            let expected = if red { [255, 0, 0] } else { [0, 0, 0] };
            assert!(near(p, expected), "x={x}: {p:?}");
        }
    }
}
