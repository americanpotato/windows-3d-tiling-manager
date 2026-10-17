//! wgpu renderer for the overlay. The swapchain lives only while the overlay is open; the
//! device-level `Gpu` is either kept warm between opens or dropped too (`keep_warm: false`),
//! in which case no GPU memory at all is held while idle.

use std::collections::HashMap;
use std::num::NonZeroIsize;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowsDisplayHandle,
};

use crate::image::Image;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    inv_view_proj: [[f32; 4]; 4],
    cam_pos: [f32; 4],
    misc: [f32; 4],
    /// w: 0 the grid room; 1 a plain colour, rgb (linear); 2 a panorama, 3 a fixed backdrop,
    /// with x the brightness and yz the backdrop's scale to cover the screen; 4 an animated
    /// scene, x the brightness and y its index in `fs_scene`.
    bg: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Inst {
    a: [f32; 4],
    b: [f32; 4],
    c: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RoomVertex {
    p: [f32; 3],
    n: [f32; 3],
}

pub struct CardDraw {
    pub center: Vec3,
    /// Half-extent vectors.
    pub right: Vec3,
    pub up: Vec3,
    pub alpha: f32,
    pub ghost: f32,
    pub highlight: f32,
    pub tex: usize,
}

pub struct HudDraw {
    /// NDC rectangle x0, y0, x1, y1.
    pub rect: [f32; 4],
    pub alpha: f32,
    pub tex: usize,
}

struct GpuTexture {
    tex: wgpu::Texture,
    bind: wgpu::BindGroup,
    w: u32,
    h: u32,
}

/// Device-level state: adapter, device, pipelines and a texture cache keyed by card id. With
/// `keep_warm` it survives while the overlay is hidden so the next open skips GPU start-up and
/// re-uploading unchanged cards; otherwise it is dropped with the overlay.
pub struct Gpu {
    // Field order matters for drop order: resources first, device/instance last.
    cache: HashMap<u64, (u64, Arc<GpuTexture>)>,
    room_buf: wgpu::Buffer,
    room_count: u32,
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    tex_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    room_pipe: wgpu::RenderPipeline,
    card_pipe: wgpu::RenderPipeline,
    hud_pipe: wgpu::RenderPipeline,
    sky_pipe: wgpu::RenderPipeline,
    scene_pipe: wgpu::RenderPipeline,
    /// Wraps around horizontally (panoramas).
    sky_sampler: wgpu::Sampler,
    /// The background picture: content key, panorama or not, texture.
    sky: Option<(u64, bool, Arc<GpuTexture>)>,
    format: wgpu::TextureFormat,
    samples: u32,
    want_msaa: bool,
    queue: wgpu::Queue,
    device: wgpu::Device,
    adapter: wgpu::Adapter,
    instance: wgpu::Instance,
}

/// What is drawn behind the cards (see `background.rs`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Backdrop {
    /// The grid room.
    Room,
    /// A plain colour (linear rgb), with only a faint floor grid.
    Plain([f32; 3]),
    /// The picture in `Gpu::sky` (the clear colour until it is there), darkened by `dim`.
    Picture { dim: f32 },
    /// An animated scene (`background::SCENES`, by index), darkened by `dim`.
    Scene { index: u32, dim: f32 },
}

/// Per-open state: the swapchain of the overlay window and its render targets.
pub struct Renderer {
    textures: Vec<Arc<GpuTexture>>,
    inst_buf: wgpu::Buffer,
    inst_cap: usize,
    depth: wgpu::TextureView,
    msaa: Option<wgpu::TextureView>,
    /// Selftest: read the next frame back into `captured`.
    pub want_capture: bool,
    /// Selftest: time each frame's render pass on the GPU (waits for it) into `gpu_ms`.
    pub gpu_timing: bool,
    pub gpu_ms: Vec<f32>,
    timer: Option<(wgpu::QuerySet, wgpu::Buffer, wgpu::Buffer)>,
    timer_n: u32,
    pub captured: Option<crate::image::Image>,
    config: wgpu::SurfaceConfiguration,
    surface: wgpu::Surface<'static>,
    gpu: Gpu,
    /// A texture or the surface changed since the last frame, so the next one must be drawn.
    dirty: std::cell::Cell<bool>,
    background: Backdrop,
}

/// Frames per batch of GPU timestamps (see `gpu_timing`).
const TIMER_FRAMES: u32 = 64;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
pub const ROOM_HALF: f32 = 16.0;
pub const ROOM_HEIGHT: f32 = 9.0;

static LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Was a GPU device lost (or hit an unrecoverable error) since the last call? The kept
/// adapter is forgotten too, so the next device is made from scratch.
pub fn take_lost() -> bool {
    let lost = LOST.swap(false, std::sync::atomic::Ordering::Relaxed);
    if lost {
        forget_adapter();
    }
    lost
}

/// For the self-test: behave as if the device was lost.
pub fn mark_lost() {
    LOST.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Drops the adapter kept for fast re-opens (e.g. the monitors or the GPU changed).
pub fn forget_adapter() {
    if let Ok(mut k) = KEPT.lock() {
        *k = None;
    }
}

static KEPT: std::sync::Mutex<Option<(wgpu::Instance, wgpu::Adapter)>> =
    std::sync::Mutex::new(None);

/// `gpu` settings that are not graphics card names.
pub const GPU_KEYWORDS: [&str; 4] = ["low-power", "high-performance", "default", "software"];

/// The DirectX 12 graphics card named `gpu` (not a keyword), if there is one that can show
/// on `surface`.
fn named_adapter(
    instance: &wgpu::Instance,
    gpu: &str,
    surface: Option<&wgpu::Surface>,
) -> Option<wgpu::Adapter> {
    if gpu.is_empty() || GPU_KEYWORDS.contains(&gpu) {
        return None;
    }
    let found = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12))
        .into_iter()
        .filter(|a| a.get_info().device_type != wgpu::DeviceType::Cpu)
        .find(|a| {
            a.get_info().name.eq_ignore_ascii_case(gpu)
                && surface.is_none_or(|s| a.is_surface_supported(s))
        });
    if found.is_none() {
        crate::store::log(&format!("graphics card \"{gpu}\" not found: picking one by power preference"));
    }
    found
}

/// Names of the DirectX 12 graphics cards (not the software renderer), for the options menu.
pub fn adapter_names() -> Vec<String> {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES
        .get_or_init(|| {
            let (instance, _) = base_instance(false);
            let mut names: Vec<String> =
                pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12))
                    .into_iter()
                    .map(|a| a.get_info())
                    .filter(|i| i.device_type != wgpu::DeviceType::Cpu)
                    .map(|i| i.name)
                    .collect();
            names.dedup();
            names
        })
        .clone()
}

/// A fresh instance, or the instance and adapter kept from a previous open (`keep_gpu_adapter`).
fn base_instance(keep_adapter: bool) -> (wgpu::Instance, Option<wgpu::Adapter>) {
    if keep_adapter {
        if let Some((i, a)) = KEPT.lock().ok().and_then(|k| k.clone()) {
            return (i, Some(a));
        }
    }
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::DX12;
    desc.flags = if cfg!(debug_assertions) {
        wgpu::InstanceFlags::debugging()
    } else {
        wgpu::InstanceFlags::empty()
    };
    (wgpu::Instance::new(desc), None)
}

/// The swapchain format we render to: the first sRGB one the surface offers.
fn pick_format(surface: &wgpu::Surface, adapter: &wgpu::Adapter) -> Option<wgpu::TextureFormat> {
    let caps = surface.get_capabilities(adapter);
    caps.formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .or_else(|| caps.formats.first().copied())
}

impl Gpu {
    /// Starts the GPU without a window (background warm-up). The swapchain format is assumed to
    /// be the usual DX12 one and checked when the overlay opens.
    pub fn new(want_msaa: bool, gpu: &str, keep_adapter: bool) -> Result<Self, String> {
        let (instance, kept) = base_instance(keep_adapter);
        Self::create(instance, kept, None, want_msaa, gpu, keep_adapter)
    }

    /// Whether this device can render to `surface` with the current settings.
    fn fits(&self, surface: &wgpu::Surface, want_msaa: bool) -> bool {
        self.want_msaa == want_msaa
            && self.adapter.is_surface_supported(surface)
            && pick_format(surface, &self.adapter) == Some(self.format)
    }

    pub fn poll(&self) {
        let _ = self.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_secs(2)),
        });
    }

    /// Bytes of texture memory held by the cache (including mips).
    pub fn cache_bytes(&self) -> u64 {
        self.cache
            .values()
            .map(|(_, t)| t.w as u64 * t.h as u64 * 4 * 4 / 3)
            .sum()
    }

    /// Whether the texture for card `id` with content `key` is cached.
    pub fn has_cached(&self, id: u64, key: u64) -> bool {
        self.cache.get(&id).is_some_and(|(k, _)| *k == key)
    }

    /// Uploads card `id` with content `key` into the cache ahead of the next open.
    pub fn preload(&mut self, id: u64, key: u64, img: &Image) {
        let t = self.upload(img, true);
        self.cache.insert(id, (key, t));
    }

    /// Drops cached textures whose ids fail `keep`.
    pub fn retain_cache(&mut self, keep: impl Fn(u64) -> bool) {
        self.cache.retain(|id, _| keep(*id));
    }

    fn create(
        instance: wgpu::Instance,
        kept: Option<wgpu::Adapter>,
        surface: Option<&wgpu::Surface>,
        want_msaa: bool,
        gpu: &str,
        keep_adapter: bool,
    ) -> Result<Self, String> {
        let t0 = std::time::Instant::now();
        let adapter = match kept {
            Some(a) if surface.is_none_or(|s| a.is_surface_supported(s)) => a,
            // A graphics card picked by name (the options menu).
            _ if let Some(a) = named_adapter(&instance, gpu, surface) => a,
            _ => {
                let request = |fallback| {
                    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: match gpu {
                            "high-performance" => wgpu::PowerPreference::HighPerformance,
                            "default" => wgpu::PowerPreference::None,
                            _ => wgpu::PowerPreference::LowPower,
                        },
                        force_fallback_adapter: fallback,
                        compatible_surface: surface,
                        apply_limit_buckets: false,
                    }))
                };
                // No DirectX 12 graphics card (old GPU, virtual machine, remote desktop):
                // Windows' software renderer (WARP) is slow but works.
                request(gpu == "software")
                    .or_else(|e| {
                        crate::store::log(&format!(
                            "no DirectX 12 GPU ({e}): using the software renderer"
                        ));
                        request(true)
                    })
                    .map_err(|e| {
                        format!(
                            "No DirectX 12 graphics adapter was found ({e}). Updating the graphics driver may help."
                        )
                    })?
            }
        };
        if keep_adapter {
            if let Ok(mut k) = KEPT.lock() {
                *k = Some((instance.clone(), adapter.clone()));
            }
        }
        let t_adapter = t0.elapsed().as_millis();

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("window3d"),
            // Only used when the selftest measures GPU time per frame.
            required_features: adapter.features() & wgpu::Features::TIMESTAMP_QUERY,
            required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..Default::default()
        }))
        .map_err(|e| format!("request_device: {e}"))?;
        // A lost device (driver update or reset, sleep, GPU removed) must not abort the app:
        // the next frame sees `take_lost` and the overlay is rebuilt on a new device.
        device.set_device_lost_callback(|reason, msg| {
            crate::store::log(&format!("GPU device lost ({reason:?}): {msg}"));
            LOST.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        device.on_uncaptured_error(Arc::new(|e: wgpu::Error| {
            static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
                crate::store::log(&format!("GPU error: {e}"));
            }
            if !matches!(e, wgpu::Error::Validation { .. }) {
                LOST.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }));
        let t_device = t0.elapsed().as_millis();

        let format = match surface {
            Some(s) => pick_format(s, &adapter).ok_or("surface has no formats")?,
            None => wgpu::TextureFormat::Bgra8UnormSrgb,
        };
        let samples = if want_msaa
            && adapter
                .get_texture_format_features(format)
                .flags
                .sample_count_supported(4)
            && adapter
                .get_texture_format_features(DEPTH_FORMAT)
                .flags
                .sample_count_supported(4)
        {
            4
        } else {
            1
        };

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shader"),
            source: wgpu::ShaderSource::Wgsl(concat!(include_str!("shader.wgsl"), include_str!("scene.wgsl")).into()),
        });

        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture"),
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

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 8,
            ..Default::default()
        });

        let sky_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sky"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("layout"),
            bind_group_layouts: &[Some(&globals_layout), Some(&tex_layout)],
            immediate_size: 0,
        });
        let room_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("room layout"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });

        let inst_attrs = wgpu::vertex_attr_array![2 => Float32x4, 3 => Float32x4, 4 => Float32x4];
        let inst_layout = wgpu::VertexBufferLayout {
            array_stride: size_of::<Inst>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &inst_attrs,
        };
        let room_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
        let room_vlayout = wgpu::VertexBufferLayout {
            array_stride: size_of::<RoomVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &room_attrs,
        };

        let blend = Some(wgpu::BlendState::ALPHA_BLENDING);
        let make = |label: &str,
                    layout: &wgpu::PipelineLayout,
                    vs: &str,
                    fs: &str,
                    buffers: &[Option<wgpu::VertexBufferLayout>],
                    depth_write: bool,
                    depth_cmp: wgpu::CompareFunction,
                    blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState {
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(depth_write),
                    depth_compare: Some(depth_cmp),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let room_pipe = make(
            "room",
            &room_layout,
            "vs_room",
            "fs_room",
            &[Some(room_vlayout)],
            true,
            wgpu::CompareFunction::Less,
            None,
        );
        let card_pipe = make(
            "card",
            &layout,
            "vs_card",
            "fs_card",
            &[Some(inst_layout.clone())],
            true,
            wgpu::CompareFunction::Less,
            blend,
        );
        let hud_pipe = make(
            "hud",
            &layout,
            "vs_hud",
            "fs_hud",
            &[Some(inst_layout)],
            false,
            wgpu::CompareFunction::Always,
            blend,
        );
        let sky_pipe = make(
            "sky",
            &layout,
            "vs_sky",
            "fs_sky",
            &[],
            false,
            wgpu::CompareFunction::Always,
            None,
        );

        let scene_pipe = make(
            "scene",
            &room_layout,
            "vs_sky",
            "fs_scene",
            &[],
            false,
            wgpu::CompareFunction::Always,
            None,
        );

        let room = room_vertices();
        let room_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("room"),
            size: (room.len() * size_of::<RoomVertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&room_buf, 0, bytemuck::cast_slice(&room));

        crate::store::log(&format!(
            "gpu: {} | adapter={t_adapter} device={t_device} pipelines={} ms",
            adapter.get_info().name,
            t0.elapsed().as_millis()
        ));
        Ok(Self {
            cache: HashMap::new(),
            room_buf,
            room_count: room.len() as u32,
            globals_buf,
            globals_bind,
            tex_layout,
            sampler,
            room_pipe,
            card_pipe,
            hud_pipe,
            sky_pipe,
            scene_pipe,
            sky_sampler,
            sky: None,
            format,
            samples,
            want_msaa,
            queue,
            device,
            adapter,
            instance,
        })
    }

    /// Uploads an sRGB BGRA image, optionally with a full CPU-generated mip chain.
    fn upload(&self, img: &Image, mips: bool) -> Arc<GpuTexture> {
        let max_dim = self.device.limits().max_texture_dimension_2d;
        let levels = if mips {
            32 - img.w.max(img.h).leading_zeros()
        } else {
            1
        };
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: img.w.min(max_dim),
                height: img.h.min(max_dim),
                depth_or_array_layers: 1,
            },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut level_img: Option<Image> = None;
        for level in 0..levels {
            let cur = level_img.as_ref().unwrap_or(img);
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &cur.px,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(cur.w * 4),
                    rows_per_image: Some(cur.h),
                },
                wgpu::Extent3d {
                    width: cur.w,
                    height: cur.h,
                    depth_or_array_layers: 1,
                },
            );
            if level + 1 < levels {
                level_img = Some(cur.half());
            }
        }
        self.wrap(tex, img.w, img.h)
    }

    /// The largest texture side the device takes.
    pub fn max_texture_dim(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    /// Content key of the background picture on the GPU.
    pub fn sky_key(&self) -> Option<u64> {
        self.sky.as_ref().map(|s| s.0)
    }

    /// Uploads a background picture (its levels from `background::load`).
    pub fn set_sky(&mut self, l: &crate::background::Loaded) {
        let img = &l.levels[0];
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sky"),
            size: wgpu::Extent3d {
                width: img.w,
                height: img.h,
                depth_or_array_layers: 1,
            },
            mip_level_count: l.levels.len() as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level, cur) in l.levels.iter().enumerate() {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &cur.px,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(cur.w * 4),
                    rows_per_image: Some(cur.h),
                },
                wgpu::Extent3d {
                    width: cur.w,
                    height: cur.h,
                    depth_or_array_layers: 1,
                },
            );
        }
        let view = tex.create_view(&Default::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sky"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sky_sampler),
                },
            ],
        });
        self.sky = Some((l.key, l.pano, Arc::new(GpuTexture { tex, bind, w: img.w, h: img.h })));
    }

    fn wrap(&self, tex: wgpu::Texture, w: u32, h: u32) -> Arc<GpuTexture> {
        let view = tex.create_view(&Default::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        Arc::new(GpuTexture { tex, bind, w, h })
    }
}

/// A live texture ready to be put in a slot (see `Renderer::live_wrap`).
pub struct LiveTex(Arc<GpuTexture>);

/// What live capture needs to share textures with wgpu (see `Renderer::share`).
pub struct Share {
    pub device: wgpu::Device,
    pub d3d12: windows::Win32::Graphics::Direct3D12::ID3D12Device,
    pub queue: windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue,
    pub luid: windows::Win32::Foundation::LUID,
}

/// D3D12 devices and queues are free-threaded.
unsafe impl Send for Share {}

/// Makes textures for live window content (see `Renderer::texture_maker`).
#[derive(Clone)]
pub struct TextureMaker(wgpu::Device);

impl TextureMaker {
    pub fn make(&self, w: u32, h: u32) -> wgpu::Texture {
        let max_dim = self.0.limits().max_texture_dimension_2d;
        let (w, h) = (w.clamp(1, max_dim), h.clamp(1, max_dim));
        self.0.create_texture(&wgpu::TextureDescriptor {
            label: Some("live"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }
}

impl Renderer {
    /// Opens a swapchain on the overlay window, reusing `gpu` when it is still suitable.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gpu: Option<Gpu>,
        hwnd: isize,
        hinstance: isize,
        width: u32,
        height: u32,
        want_msaa: bool,
        gpu_pref: &str,
        keep_adapter: bool,
    ) -> Result<Self, String> {
        let t0 = std::time::Instant::now();
        let (instance, kept) = match &gpu {
            Some(g) => (g.instance.clone(), None),
            None => base_instance(keep_adapter),
        };
        let mut wh = Win32WindowHandle::new(NonZeroIsize::new(hwnd).ok_or("null hwnd")?);
        wh.hinstance = NonZeroIsize::new(hinstance);
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new())),
                raw_window_handle: RawWindowHandle::Win32(wh),
            })
        }
        .map_err(|e| format!("create_surface: {e}"))?;

        let t_surface = t0.elapsed().as_millis();
        let warm = gpu.is_some();
        let gpu = match gpu {
            Some(g) if g.fits(&surface, want_msaa) => g,
            other => {
                if warm {
                    crate::store::log("kept GPU does not fit the overlay surface; recreating");
                }
                let kept = kept.or_else(|| other.as_ref().map(|g| g.adapter.clone()));
                drop(other);
                Gpu::create(
                    instance,
                    kept,
                    Some(&surface),
                    want_msaa,
                    gpu_pref,
                    keep_adapter,
                )?
            }
        };

        let caps = surface.get_capabilities(&gpu.adapter);
        let mut config = surface
            .get_default_config(&gpu.adapter, width.max(1), height.max(1))
            .ok_or("surface not supported by adapter")?;
        config.format = gpu.format;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.alpha_mode = wgpu::CompositeAlphaMode::Opaque;
        config.desired_maximum_frame_latency = 2;
        if caps.usages.contains(wgpu::TextureUsages::COPY_SRC) {
            config.usage |= wgpu::TextureUsages::COPY_SRC;
        }
        let t_fit = t0.elapsed().as_millis();
        surface.configure(&gpu.device, &config);
        let t_conf = t0.elapsed().as_millis();

        let inst_cap = 64;
        let inst_buf = make_inst_buf(&gpu.device, inst_cap);
        let (depth, msaa) = make_targets(&gpu.device, &config, gpu.samples);
        crate::store::log(&format!(
            "surface ready in {} ms (gpu {}): create={t_surface} fit={t_fit} configure={t_conf}",
            t0.elapsed().as_millis(),
            if warm { "warm" } else { "cold" }
        ));
        Ok(Self {
            textures: Vec::new(),
            inst_buf,
            inst_cap,
            depth,
            msaa,
            want_capture: false,
            gpu_timing: false,
            gpu_ms: Vec::new(),
            timer: None,
            timer_n: 0,
            captured: None,
            config,
            surface,
            gpu,
            dirty: std::cell::Cell::new(true),
            background: Backdrop::Room,
        })
    }

    /// Releases the swapchain and per-open resources, keeping the device and texture cache.
    pub fn into_gpu(self) -> Gpu {
        self.finish();
        let Self {
            textures,
            inst_buf,
            depth,
            msaa,
            surface,
            gpu,
            ..
        } = self;
        drop((textures, inst_buf, depth, msaa, surface));
        // wgpu defers destruction until the next poll: free the swapchain and targets now.
        gpu.poll();
        gpu
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// Number of texture slots in use.
    pub fn texture_count(&self) -> usize {
        self.textures.len()
    }

    /// Releases the texture slots from `n` on (the cache keeps what it holds).
    pub fn truncate_textures(&mut self, n: usize) {
        self.dirty.set(true);
        self.textures.truncate(n);
    }

    pub fn gpu_mut(&mut self) -> &mut Gpu {
        self.dirty.set(true);
        &mut self.gpu
    }

    /// Size in pixels of the texture in `slot`.
    pub fn tex_size(&self, slot: usize) -> (u32, u32) {
        let t = &self.textures[slot];
        (t.w, t.h)
    }

    /// Uploads an image and caches it as card `id` with content `key`. Returns the texture slot.
    pub fn upload_cached(&mut self, id: u64, key: u64, img: &Image, mips: bool) -> usize {
        self.dirty.set(true);
        let t = self.gpu.upload(img, mips);
        self.gpu.cache.insert(id, (key, t.clone()));
        self.textures.push(t);
        self.textures.len() - 1
    }

    /// Creates an uncached `w`x`h` texture for live window content, in `slot` if given (replacing
    /// what was there) or in a new slot. Returns the slot.
    pub fn live_slot(&mut self, slot: Option<usize>, w: u32, h: u32) -> usize {
        let tex = self.texture_maker().make(w, h);
        self.live_slot_with(slot, tex)
    }

    /// For making live textures on another thread: creating a big one can take tens of
    /// milliseconds.
    pub fn texture_maker(&self) -> TextureMaker {
        TextureMaker(self.gpu.device.clone())
    }

    /// The D3D12 device and queue under wgpu, for live capture to hand frames over on the GPU.
    pub fn share(&self) -> Option<Share> {
        if std::env::var_os("WINDOW3D_NO_SHARE").is_some() {
            return None;
        }
        unsafe {
            let d3d12 = self
                .gpu
                .device
                .as_hal::<wgpu::hal::api::Dx12>()?
                .raw_device()
                .clone();
            let queue = self
                .gpu
                .queue
                .as_hal::<wgpu::hal::api::Dx12>()?
                .as_raw()
                .clone();
            Some(Share {
                device: self.gpu.device.clone(),
                luid: d3d12.GetAdapterLuid(),
                d3d12,
                queue,
            })
        }
    }

    /// Puts `tex` (from a `TextureMaker`) in `slot` if given, or in a new slot. Returns the slot.
    pub fn live_slot_with(&mut self, slot: Option<usize>, tex: wgpu::Texture) -> usize {
        let t = self.live_wrap(tex);
        self.live_slot_set(slot, &t)
    }

    /// `tex` (from a `TextureMaker`) made ready to be put in slots with [`Self::live_slot_set`].
    pub fn live_wrap(&self, tex: wgpu::Texture) -> LiveTex {
        let (w, h) = (tex.width(), tex.height());
        LiveTex(self.gpu.wrap(tex, w, h))
    }

    /// Puts `t` in `slot` if given, or in a new slot. Returns the slot.
    pub fn live_slot_set(&mut self, slot: Option<usize>, t: &LiveTex) -> usize {
        self.dirty.set(true);
        let t = t.0.clone();
        match slot {
            Some(s) if s < self.textures.len() => {
                self.textures[s] = t;
                s
            }
            _ => {
                self.textures.push(t);
                self.textures.len() - 1
            }
        }
    }

    /// Writes BGRA rows (`pitch` bytes apart) into the top-left of the texture in `slot`.
    pub fn write_live(&self, slot: usize, data: &[u8], pitch: u32, w: u32, h: u32) {
        self.dirty.set(true);
        let Some(t) = self.textures.get(slot) else {
            return;
        };
        let (w, h) = (w.min(t.w), h.min(t.h));
        if w == 0 || h == 0 || data.len() < (pitch * (h - 1) + w * 4) as usize {
            return;
        }
        self.gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &t.tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(pitch),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    /// The slot of the cached texture of card `id`, if its content is still `key`.
    pub fn cached(&mut self, id: u64, key: u64) -> Option<usize> {
        self.dirty.set(true);
        let t = match self.gpu.cache.get(&id) {
            Some((k, t)) if *k == key => t.clone(),
            _ => return None,
        };
        self.textures.push(t);
        Some(self.textures.len() - 1)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 || (w, h) == self.size() {
            return;
        }
        self.dirty.set(true);
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.gpu.device, &self.config);
        let (d, m) = make_targets(&self.gpu.device, &self.config, self.gpu.samples);
        self.depth = d;
        self.msaa = m;
    }

    pub fn samples(&self) -> u32 {
        self.gpu.samples
    }

    /// Forces the next frame to be drawn.
    pub fn mark_dirty(&self) {
        self.dirty.set(true);
    }

    /// Sets what is drawn behind the cards.
    pub fn set_background(&mut self, bg: Backdrop) {
        if self.background != bg {
            self.background = bg;
            self.dirty.set(true);
        }
    }

    /// The background moves by itself, so frames must keep coming.
    pub fn animated(&self) -> bool {
        matches!(self.background, Backdrop::Scene { .. })
    }

    /// Takes the flag set when a texture or the surface changed since the last call.
    pub fn take_dirty(&self) -> bool {
        self.dirty.replace(false)
    }

    /// Renders one frame (skipped if the surface is not ready).
    pub fn render(
        &mut self,
        view_proj: Mat4,
        cam_pos: Vec3,
        time: f32,
        cards: &[CardDraw],
        hud: &[HudDraw],
    ) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.gpu.device, &self.config);
                return;
            }
            _ => return,
        };

        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            cam_pos: [cam_pos.x, cam_pos.y, cam_pos.z, 1.0],
            misc: [time, (self.gpu.samples == 1) as u32 as f32, 0.0, 0.0],
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            bg: match self.background {
                Backdrop::Room => [0.0; 4],
                Backdrop::Plain([r, g, b]) => [r, g, b, 1.0],
                Backdrop::Picture { dim } => match &self.gpu.sky {
                    Some((_, true, _)) => [1.0 - dim, 1.0, 1.0, 2.0],
                    Some((_, false, t)) => {
                        // Cover the screen, cropping the longer side of the picture.
                        let screen = self.config.width as f32 / self.config.height.max(1) as f32;
                        let pic = t.w as f32 / t.h.max(1) as f32;
                        let (sx, sy) = if screen > pic { (1.0, pic / screen) } else { (screen / pic, 1.0) };
                        [1.0 - dim, sx, sy, 3.0]
                    }
                    None => [0.0; 4],
                },
                Backdrop::Scene { index, dim } => [1.0 - dim, index as f32, 0.0, 4.0],
            },
        };
        let clear = match self.background {
            Backdrop::Plain(c) => c,
            _ => [0.01, 0.012, 0.02],
        };
        self.gpu
            .queue
            .write_buffer(&self.gpu.globals_buf, 0, bytemuck::bytes_of(&globals));

        let mut insts: Vec<Inst> = Vec::with_capacity(cards.len() + hud.len());
        for c in cards {
            insts.push(Inst {
                a: [c.center.x, c.center.y, c.center.z, c.alpha],
                b: [c.right.x, c.right.y, c.right.z, c.ghost],
                c: [c.up.x, c.up.y, c.up.z, c.highlight],
            });
        }
        for h in hud {
            insts.push(Inst {
                a: h.rect,
                b: [h.alpha, 0.0, 0.0, 0.0],
                c: [0.0; 4],
            });
        }
        if insts.len() > self.inst_cap {
            self.inst_cap = insts.len().next_power_of_two();
            self.inst_buf = make_inst_buf(&self.gpu.device, self.inst_cap);
        }
        if !insts.is_empty() {
            self.gpu
                .queue
                .write_buffer(&self.inst_buf, 0, bytemuck::cast_slice(&insts));
        }

        if self.gpu_timing
            && self.timer.is_none()
            && self.gpu.device.features().contains(wgpu::Features::TIMESTAMP_QUERY)
        {
            let d = &self.gpu.device;
            let buf = |usage| {
                d.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("timestamps"),
                    size: TIMER_FRAMES as u64 * 16,
                    usage,
                    mapped_at_creation: false,
                })
            };
            self.timer = Some((
                d.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("timestamps"),
                    ty: wgpu::QueryType::Timestamp,
                    count: TIMER_FRAMES * 2,
                }),
                buf(wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC),
                buf(wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ),
            ));
        }
        let timer = self.timer.as_ref().filter(|_| self.gpu_timing);
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.gpu.device.create_command_encoder(&Default::default());
        {
            let (target, resolve) = match &self.msaa {
                Some(m) => (m, Some(&view)),
                None => (&view, None),
            };
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: resolve,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear[0] as f64,
                            g: clear[1] as f64,
                            b: clear[2] as f64,
                            a: 1.0,
                        }),
                        // The multisampled target is only needed until it is resolved.
                        store: if resolve.is_some() {
                            wgpu::StoreOp::Discard
                        } else {
                            wgpu::StoreOp::Store
                        },
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: timer.map(|t| wgpu::RenderPassTimestampWrites {
                    query_set: &t.0,
                    beginning_of_pass_write_index: Some(self.timer_n * 2),
                    end_of_pass_write_index: Some(self.timer_n * 2 + 1),
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.gpu.globals_bind, &[]);

            match (&self.background, &self.gpu.sky) {
                (Backdrop::Picture { .. }, Some((_, _, sky))) => {
                    pass.set_pipeline(&self.gpu.sky_pipe);
                    pass.set_bind_group(1, &sky.bind, &[]);
                    pass.draw(0..3, 0..1);
                }
                (Backdrop::Scene { .. }, _) => {
                    pass.set_pipeline(&self.gpu.scene_pipe);
                    pass.draw(0..3, 0..1);
                }
                // Still loading: the clear colour.
                (Backdrop::Picture { .. }, None) => {}
                _ => {
                    pass.set_pipeline(&self.gpu.room_pipe);
                    pass.set_vertex_buffer(0, self.gpu.room_buf.slice(..));
                    pass.draw(0..self.gpu.room_count, 0..1);
                }
            }

            pass.set_vertex_buffer(0, self.inst_buf.slice(..));
            pass.set_pipeline(&self.gpu.card_pipe);
            for (i, c) in cards.iter().enumerate() {
                if let Some(t) = self.textures.get(c.tex) {
                    pass.set_bind_group(1, &t.bind, &[]);
                    pass.draw(0..6, i as u32..i as u32 + 1);
                }
            }
            pass.set_pipeline(&self.gpu.hud_pipe);
            for (j, h) in hud.iter().enumerate() {
                if let Some(t) = self.textures.get(h.tex) {
                    let i = (cards.len() + j) as u32;
                    pass.set_bind_group(1, &t.bind, &[]);
                    pass.draw(0..6, i..i + 1);
                }
            }
        }
        let readback = self.want_capture.then(|| {
            let (w, h) = (frame.texture.width(), frame.texture.height());
            let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
            let buf = self.gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("readback"),
                size: (row * h) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            enc.copy_texture_to_buffer(
                frame.texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buf,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(h),
                    },
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            (buf, w, h, row)
        });
        // Timestamps are read back in batches so the GPU never idles (and clocks down)
        // between timed frames.
        let timer = timer.filter(|_| {
            self.timer_n += 1;
            self.timer_n == TIMER_FRAMES
        });
        if let Some((qs, resolve, read)) = timer {
            self.timer_n = 0;
            enc.resolve_query_set(qs, 0..TIMER_FRAMES * 2, resolve, 0);
            enc.copy_buffer_to_buffer(resolve, 0, read, 0, TIMER_FRAMES as u64 * 16);
        }
        self.gpu.queue.submit(Some(enc.finish()));
        if let Some((_, _, read)) = timer {
            read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            let _ = self.gpu.device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(5)),
            });
            if let Ok(data) = read.slice(..).get_mapped_range() {
                let period = self.gpu.queue.get_timestamp_period() as f64;
                for t in bytemuck::cast_slice::<u8, u64>(&data).chunks_exact(2) {
                    self.gpu_ms.push((t[1].wrapping_sub(t[0]) as f64 * period / 1e6) as f32);
                }
            }
            read.unmap();
        }
        if let Some((buf, w, h, row)) = readback {
            self.want_capture = false;
            buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            let _ = self.gpu.device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(5)),
            });
            if let Ok(data) = buf.slice(..).get_mapped_range() {
                let rgba = matches!(
                    self.config.format,
                    wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb
                );
                let mut img = crate::image::Image::new(w, h);
                for y in 0..h as usize {
                    let src = &data[y * row as usize..][..w as usize * 4];
                    let dst = &mut img.px[y * w as usize * 4..][..w as usize * 4];
                    dst.copy_from_slice(src);
                    if rgba {
                        dst.chunks_exact_mut(4).for_each(|p| p.swap(0, 2));
                    }
                }
                self.captured = Some(img);
            }
        }
        self.gpu.queue.present(frame);
    }

    /// Waits for the GPU to finish so that dropping releases everything immediately.
    pub fn finish(&self) {
        let _ = self.gpu.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_secs(2)),
        });
    }
}

fn make_inst_buf(device: &wgpu::Device, cap: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("instances"),
        size: (cap * size_of::<Inst>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn make_targets(
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
    samples: u32,
) -> (wgpu::TextureView, Option<wgpu::TextureView>) {
    let size = wgpu::Extent3d {
        width: config.width,
        height: config.height,
        depth_or_array_layers: 1,
    };
    let depth = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("depth"),
        size,
        mip_level_count: 1,
        sample_count: samples,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let msaa = (samples > 1).then(|| {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("msaa"),
                size,
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format: config.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&Default::default())
    });
    (depth.create_view(&Default::default()), msaa)
}

/// The background of a `background` setting: "room" (or anything unreadable) is the grid
/// room (`None`), "#rrggbb" a plain colour, returned in linear rgb.
pub fn background_color(s: &str) -> Option<[f32; 3]> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    let lin = |c: u32| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    Some([lin(v >> 16 & 255), lin(v >> 8 & 255), lin(v & 255)])
}

/// Inside faces of the room box (normals point inward).
fn room_vertices() -> Vec<RoomVertex> {
    let (h, y1) = (ROOM_HALF, ROOM_HEIGHT);
    let mut v = Vec::with_capacity(36);
    let mut quad = |a: [f32; 3], b: [f32; 3], c: [f32; 3], d: [f32; 3], n: [f32; 3]| {
        for p in [a, b, c, a, c, d] {
            v.push(RoomVertex { p, n });
        }
    };
    quad(
        [-h, 0.0, -h],
        [h, 0.0, -h],
        [h, 0.0, h],
        [-h, 0.0, h],
        [0.0, 1.0, 0.0],
    ); // floor
    quad(
        [-h, y1, -h],
        [h, y1, -h],
        [h, y1, h],
        [-h, y1, h],
        [0.0, -1.0, 0.0],
    ); // ceiling
    quad(
        [-h, 0.0, -h],
        [h, 0.0, -h],
        [h, y1, -h],
        [-h, y1, -h],
        [0.0, 0.0, 1.0],
    ); // back
    quad(
        [-h, 0.0, h],
        [h, 0.0, h],
        [h, y1, h],
        [-h, y1, h],
        [0.0, 0.0, -1.0],
    ); // front
    quad(
        [-h, 0.0, -h],
        [-h, 0.0, h],
        [-h, y1, h],
        [-h, y1, -h],
        [1.0, 0.0, 0.0],
    ); // left
    quad(
        [h, 0.0, -h],
        [h, 0.0, h],
        [h, y1, h],
        [h, y1, -h],
        [-1.0, 0.0, 0.0],
    ); // right
    v
}
