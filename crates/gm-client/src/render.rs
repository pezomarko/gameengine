//! wgpu device setup and the renderer: the world in one draw call with PVS-culled indices,
//! entity boxes in one more, and one per character (`characters`).

use glam::{Mat4, Vec3};

use crate::Error;
use crate::characters::{CharacterDraw, Characters};
use crate::hud::Hud;
use crate::world::{FaceRange, TEXTURE_SIZE, Vertex, WorldMesh};

pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const FOV_Y_DEG: f32 = 75.0;
const NEAR: f32 = 4.0;
const FAR: f32 = 8192.0;

pub struct Gpu {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub info: wgpu::AdapterInfo,
    /// BC texture compression is available: model atlases upload as they are (MODELS.md 8).
    pub bc: bool,
}

impl Gpu {
    /// A buffer holding `contents`, filled through the queue. Not mapped at creation: Chrome's
    /// CPU adapter (the software WebGPU the browser gates run on) refuses a mapped-at-creation
    /// buffer past a size the town's vertices exceed ("too large for the implementation"), and
    /// a queue write carries any size.
    pub fn buffer(&self, label: &str, contents: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
        let size = (contents.len() as u64 + 3) & !3;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if size == contents.len() as u64 {
            self.queue.write_buffer(&buffer, 0, contents);
        } else if !contents.is_empty() {
            let mut padded = contents.to_vec();
            padded.resize(size as usize, 0);
            self.queue.write_buffer(&buffer, 0, &padded);
        }
        buffer
    }
}

impl Gpu {
    /// Blocking, for programs with a thread to block (everything but a browser).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        software: bool,
    ) -> Result<Gpu, Error> {
        pollster::block_on(Gpu::request(instance, surface, software))
    }

    pub async fn request(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        software: bool,
    ) -> Result<Gpu, Error> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::from_env()
                    .unwrap_or(wgpu::PowerPreference::HighPerformance),
                force_fallback_adapter: software,
                compatible_surface: surface,
                ..Default::default()
            })
            .await
            .map_err(|e| {
                format!("no compatible GPU adapter (is a Vulkan driver installed?): {e}")
            })?;
        let info = adapter.get_info();
        log::info!(
            "adapter: {} ({:?}, {:?}, driver {} {})",
            info.name,
            info.backend,
            info.device_type,
            info.driver,
            info.driver_info
        );
        let bc = adapter
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_BC);
        if !bc {
            log::warn!(
                "no BC texture compression on this GPU: model atlases are decoded on the CPU"
            );
        }
        // The renderer stays inside the downlevel limits (PLAN.md 11.2); WebGL2 has a
        // smaller set of its own (no storage buffers, no compute), and asking it for more
        // fails the request.
        let limits = if info.backend == wgpu::Backend::Gl {
            wgpu::Limits::downlevel_webgl2_defaults()
        } else {
            wgpu::Limits::downlevel_defaults()
        }
        .using_resolution(adapter.limits());
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("gm-client"),
                required_features: if bc {
                    wgpu::Features::TEXTURE_COMPRESSION_BC
                } else {
                    wgpu::Features::empty()
                },
                required_limits: limits,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|e| format!("device creation failed: {e}"))?;
        Ok(Gpu {
            adapter,
            device,
            queue,
            info,
            bc,
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    /// x = lightmap scale; the rest is reserved.
    params: [f32; 4],
}

/// View-projection for a Quake-convention camera (Z up, yaw counter-clockwise from +X, positive
/// pitch looks down), producing wgpu clip space.
pub fn view_proj(eye: Vec3, yaw_deg: f32, pitch_deg: f32, aspect: f32) -> Mat4 {
    view_proj_zoomed(eye, yaw_deg, pitch_deg, aspect, 1.0)
}

/// As [`view_proj`], with the field of view divided by `zoom` (a scope, MODES.md 3.2).
pub fn view_proj_zoomed(eye: Vec3, yaw_deg: f32, pitch_deg: f32, aspect: f32, zoom: f32) -> Mat4 {
    let (sy, cy) = yaw_deg.to_radians().sin_cos();
    let (sp, cp) = pitch_deg.to_radians().sin_cos();
    let forward = Vec3::new(cp * cy, cp * sy, -sp);
    let fov = (FOV_Y_DEG / zoom.max(1.0)).to_radians();
    let proj = glam::camera::rh::proj::directx::perspective(fov, aspect, NEAR, FAR);
    proj * glam::camera::rh::view::look_to_mat4(eye, forward, Vec3::Z)
}

/// The vertical field of view in degrees, for what the HUD draws in angles.
pub const fn fov_y_deg() -> f32 {
    FOV_Y_DEG
}

/// A solid box: projectiles, areas, markers. Players are `characters::CharacterDraw`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityDraw {
    pub mins: Vec3,
    pub maxs: Vec3,
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct EntityVertex {
    pos: [f32; 3],
    color: [f32; 4],
}

const ENTITY_SHADER: &str = r#"
struct Globals { view_proj: mat4x4<f32>, params: vec4<f32> };
@group(0) @binding(0) var<uniform> globals: Globals;
struct VsIn { @location(0) pos: vec3<f32>, @location(1) color: vec4<f32> };
struct VsOut { @builtin(position) clip: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = globals.view_proj * vec4<f32>(in.pos, 1.0);
    out.color = in.color;
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> { return in.color; }
"#;

/// 36 vertices of a box, faces shaded by orientation so edges read without lighting.
fn box_vertices(out: &mut Vec<EntityVertex>, d: &EntityDraw) {
    let (a, b) = (d.mins, d.maxs);
    let c = |p: Vec3| [p.x, p.y, p.z];
    let shade = |f: f32| [d.color[0] * f, d.color[1] * f, d.color[2] * f, d.color[3]];
    // Each face: 4 corners counter-clockwise seen from outside, and a shade.
    let faces: [([Vec3; 4], f32); 6] = [
        // +z top
        (
            [
                Vec3::new(a.x, a.y, b.z),
                Vec3::new(b.x, a.y, b.z),
                Vec3::new(b.x, b.y, b.z),
                Vec3::new(a.x, b.y, b.z),
            ],
            1.0,
        ),
        // -z bottom
        (
            [
                Vec3::new(a.x, b.y, a.z),
                Vec3::new(b.x, b.y, a.z),
                Vec3::new(b.x, a.y, a.z),
                Vec3::new(a.x, a.y, a.z),
            ],
            0.35,
        ),
        // +x
        (
            [
                Vec3::new(b.x, a.y, a.z),
                Vec3::new(b.x, b.y, a.z),
                Vec3::new(b.x, b.y, b.z),
                Vec3::new(b.x, a.y, b.z),
            ],
            0.8,
        ),
        // -x
        (
            [
                Vec3::new(a.x, b.y, a.z),
                Vec3::new(a.x, a.y, a.z),
                Vec3::new(a.x, a.y, b.z),
                Vec3::new(a.x, b.y, b.z),
            ],
            0.6,
        ),
        // +y
        (
            [
                Vec3::new(b.x, b.y, a.z),
                Vec3::new(a.x, b.y, a.z),
                Vec3::new(a.x, b.y, b.z),
                Vec3::new(b.x, b.y, b.z),
            ],
            0.7,
        ),
        // -y
        (
            [
                Vec3::new(a.x, a.y, a.z),
                Vec3::new(b.x, a.y, a.z),
                Vec3::new(b.x, a.y, b.z),
                Vec3::new(a.x, a.y, b.z),
            ],
            0.5,
        ),
    ];
    for (corners, f) in faces {
        let col = shade(f);
        for &i in &[0usize, 1, 2, 0, 2, 3] {
            out.push(EntityVertex {
                pos: c(corners[i]),
                color: col,
            });
        }
    }
}

/// GPU resources of one map.
struct WorldGpu {
    textures_bg: wgpu::BindGroup,
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    index_count: u32,
    face_ranges: Vec<FaceRange>,
    all_indices: Vec<u32>,
    scratch: Vec<u32>,
    faces_drawn: usize,
}

pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    entity_pipeline: wgpu::RenderPipeline,
    entity_buf: wgpu::Buffer,
    entity_capacity: usize,
    entity_vertices: Vec<EntityVertex>,
    /// The fight's effects (LOOK.md 13): see-through triangles drawn after the bodies,
    /// blended, tested against the depth and not written to it. The frame fills `fx`.
    fx_pipeline: wgpu::RenderPipeline,
    fx_buf: wgpu::Buffer,
    fx_capacity: usize,
    pub fx: crate::fx::FxMesh,
    fx_drawn: u32,
    globals_buf: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,
    /// The paperdoll's camera (LOOK.md 5), its own globals.
    doll_globals_buf: wgpu::Buffer,
    doll_globals_bg: wgpu::BindGroup,
    textures_layout: wgpu::BindGroupLayout,
    diffuse_sampler: wgpu::Sampler,
    lightmap_sampler: wgpu::Sampler,
    world: WorldGpu,
    depth_view: wgpu::TextureView,
    depth_size: (u32, u32),
    /// Models, mannequins and this frame's skinning blocks.
    pub characters: Characters,
    /// Text and bars over the frame; filled by the caller before `render`.
    pub hud: Hud,
    pub lightmap_scale: f32,
    pub faces_drawn: usize,
    /// Draw calls of the last frame: the world, the boxes, one per character, the HUD.
    pub draw_calls: usize,
}

fn make_depth(device: &wgpu::Device, size: (u32, u32)) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d {
                width: size.0.max(1),
                height: size.1.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

/// Box-filter mip chain for an RGBA image.
pub fn mip_chain(rgba: &[u8], mut w: u32, mut h: u32) -> Vec<(u32, u32, Vec<u8>)> {
    let mut levels = vec![(w, h, rgba.to_vec())];
    while w > 1 || h > 1 {
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let src = &levels.last().unwrap().2;
        let mut dst = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            for x in 0..nw {
                for c in 0..4 {
                    let mut acc = 0u32;
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let sx = (x * 2 + dx).min(w - 1);
                        let sy = (y * 2 + dy).min(h - 1);
                        acc += src[((sy * w + sx) * 4 + c) as usize] as u32;
                    }
                    dst[((y * nw + x) * 4 + c) as usize] = (acc / 4) as u8;
                }
            }
        }
        levels.push((nw, nh, dst));
        w = nw;
        h = nh;
    }
    levels
}

fn upload_layer(
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    layer: u32,
    mips: &[(u32, u32, Vec<u8>)],
) {
    for (level, (w, h, data)) in mips.iter().enumerate() {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: tex,
                mip_level: level as u32,
                origin: wgpu::Origin3d {
                    x: 0,
                    y: 0,
                    z: layer,
                },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(*h),
            },
            wgpu::Extent3d {
                width: *w,
                height: *h,
                depth_or_array_layers: 1,
            },
        );
    }
}

/// How many layers the texture array of a map with `textures` textures gets. wgpu's GL
/// backend (WebGL2) cannot be told what a texture will be viewed as and guesses from the
/// count: one layer is a plain texture, six a cube, a larger multiple of six a cube array,
/// and the array view of any of those draws nothing (the town, with its twelve textures,
/// was black in that build). So the array never has such a count: a layer nobody samples
/// is added.
fn array_layers(textures: usize) -> u32 {
    let n = textures.max(2) as u32;
    if n.is_multiple_of(6) { n + 1 } else { n }
}

/// Upload one map: its texture array, its lightmap atlas, its vertices and indices.
fn world_gpu(
    gpu: &Gpu,
    textures_layout: &wgpu::BindGroupLayout,
    diffuse_sampler: &wgpu::Sampler,
    lightmap_sampler: &wgpu::Sampler,
    world: &WorldMesh,
) -> WorldGpu {
    let device = &gpu.device;
    // Diffuse texture array with a full mip chain.
    let layers = array_layers(world.texture_layers.len());
    let mip_count = TEXTURE_SIZE.ilog2() + 1;
    let diffuse = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("diffuse array"),
        size: wgpu::Extent3d {
            width: TEXTURE_SIZE,
            height: TEXTURE_SIZE,
            depth_or_array_layers: layers,
        },
        mip_level_count: mip_count,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (i, layer) in world.texture_layers.iter().enumerate() {
        upload_layer(
            &gpu.queue,
            &diffuse,
            i as u32,
            &mip_chain(layer, TEXTURE_SIZE, TEXTURE_SIZE),
        );
    }
    let diffuse_view = diffuse.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    // Lightmap atlas, linear (not sRGB): light values multiply the albedo.
    let lm = &world.lightmap;
    let lightmap = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lightmap atlas"),
        size: wgpu::Extent3d {
            width: lm.width,
            height: lm.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    upload_layer(
        &gpu.queue,
        &lightmap,
        0,
        &[(lm.width, lm.height, lm.rgba.clone())],
    );
    let lightmap_view = lightmap.create_view(&wgpu::TextureViewDescriptor::default());
    let textures_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("textures"),
        layout: textures_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&diffuse_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(diffuse_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&lightmap_view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Sampler(lightmap_sampler),
            },
        ],
    });
    let vertex_buf = gpu.buffer(
        "world vertices",
        bytemuck::cast_slice(&world.vertices),
        wgpu::BufferUsages::VERTEX,
    );
    let index_buf = gpu.buffer(
        "world indices",
        bytemuck::cast_slice(&world.indices),
        wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
    );
    WorldGpu {
        textures_bg,
        vertex_buf,
        index_buf,
        index_count: world.indices.len() as u32,
        face_ranges: world.face_ranges.clone(),
        all_indices: world.indices.clone(),
        scratch: Vec::with_capacity(world.indices.len()),
        faces_drawn: world
            .face_ranges
            .iter()
            .filter(|r| r.index_count > 0)
            .count(),
    }
}

impl Renderer {
    pub fn new(
        gpu: &Gpu,
        color_format: wgpu::TextureFormat,
        world: &WorldMesh,
        size: (u32, u32),
    ) -> Renderer {
        let device = &gpu.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("world"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let diffuse_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("diffuse"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            lod_min_clamp: 0.0,
            lod_max_clamp: 32.0,
            compare: None,
            anisotropy_clamp: 1,
            border_color: None,
        });
        let lightmap_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("lightmap"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            lod_min_clamp: 0.0,
            lod_max_clamp: 0.0,
            compare: None,
            anisotropy_clamp: 1,
            border_color: None,
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
        let tex_entry = |binding, dim| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: dim,
                multisampled: false,
            },
            count: None,
        };
        let sampler_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let textures_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("textures"),
            entries: &[
                tex_entry(0, wgpu::TextureViewDimension::D2Array),
                sampler_entry(1),
                tex_entry(2, wgpu::TextureViewDimension::D2),
                sampler_entry(3),
            ],
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });
        let doll_globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("paperdoll globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let doll_globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("paperdoll globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: doll_globals_buf.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("world"),
            bind_group_layouts: &[Some(&globals_layout), Some(&textures_layout)],
            immediate_size: 0,
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2, 2 => Float32x2, 3 => Uint32],
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("world"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(vertex_layout)],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let entity_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("entities"),
            source: wgpu::ShaderSource::Wgsl(ENTITY_SHADER.into()),
        });
        let entity_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("entities"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });
        let entity_vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<EntityVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
        };
        let entity_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("entities"),
            layout: Some(&entity_layout),
            vertex: wgpu::VertexState {
                module: &entity_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(entity_vertex_layout)],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &entity_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        // The effects: the entities' shader and vertex, blended over what is there.
        let fx_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("effects"),
            layout: Some(&entity_layout),
            vertex: wgpu::VertexState {
                module: &entity_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<crate::fx::FxVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &entity_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let fx_capacity = 3 * 2048;
        let fx_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("effect vertices"),
            size: (fx_capacity * std::mem::size_of::<crate::fx::FxVertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let entity_capacity = 36 * 64;
        let entity_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("entity vertices"),
            size: (entity_capacity * std::mem::size_of::<EntityVertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let characters = Characters::new(gpu, color_format, &globals_layout);
        let hud = Hud::new(gpu, color_format);
        let world = world_gpu(
            gpu,
            &textures_layout,
            &diffuse_sampler,
            &lightmap_sampler,
            world,
        );
        Renderer {
            pipeline,
            entity_pipeline,
            entity_buf,
            entity_capacity,
            entity_vertices: Vec::new(),
            fx_pipeline,
            fx_buf,
            fx_capacity,
            fx: Default::default(),
            fx_drawn: 0,
            globals_buf,
            globals_bg,
            doll_globals_buf,
            doll_globals_bg,
            textures_layout,
            diffuse_sampler,
            lightmap_sampler,
            faces_drawn: world.faces_drawn,
            world,
            depth_view: make_depth(device, size),
            depth_size: size,
            characters,
            hud,
            lightmap_scale: 2.0,
            draw_calls: 0,
        }
    }

    /// Replace the map; loaded models and mannequins stay (a zone change keeps its avatars).
    pub fn set_world(&mut self, gpu: &Gpu, world: &WorldMesh) {
        self.world = world_gpu(
            gpu,
            &self.textures_layout,
            &self.diffuse_sampler,
            &self.lightmap_sampler,
            world,
        );
        self.faces_drawn = self.world.faces_drawn;
    }

    pub fn resize(&mut self, gpu: &Gpu, size: (u32, u32)) {
        if size != self.depth_size && size.0 > 0 && size.1 > 0 {
            self.depth_view = make_depth(&gpu.device, size);
            self.depth_size = size;
        }
    }

    /// Restrict drawing to `faces` (from the PVS), or to everything with `None`.
    pub fn set_visible_faces(&mut self, gpu: &Gpu, faces: Option<&[u32]>) {
        let w = &mut self.world;
        w.scratch.clear();
        let mut drawn = 0;
        match faces {
            None => {
                w.scratch.extend_from_slice(&w.all_indices);
                drawn = w.face_ranges.iter().filter(|r| r.index_count > 0).count();
            }
            Some(faces) => {
                for &f in faces {
                    let r = w.face_ranges[f as usize];
                    if r.index_count > 0 {
                        drawn += 1;
                        let start = r.first_index as usize;
                        w.scratch.extend_from_slice(
                            &w.all_indices[start..start + r.index_count as usize],
                        );
                    }
                }
            }
        }
        w.index_count = w.scratch.len() as u32;
        w.faces_drawn = drawn;
        self.faces_drawn = drawn;
        if !w.scratch.is_empty() {
            gpu.queue
                .write_buffer(&w.index_buf, 0, bytemuck::cast_slice(&w.scratch));
        }
    }

    /// Record and submit one frame into `target`: the world, `entities` and `characters`.
    pub fn render(
        &mut self,
        gpu: &Gpu,
        target: &wgpu::TextureView,
        view_proj: Mat4,
        entities: &[EntityDraw],
        characters: &[CharacterDraw],
    ) {
        self.render_with_doll(gpu, target, view_proj, entities, characters, None);
    }

    /// The same with a paperdoll (LOOK.md 5): bodies drawn into a rectangle of the screen
    /// (pixels, from the top left) with a camera of their own, between the HUD's plates
    /// and its ink, in a second pass whose depth is cleared.
    pub fn render_with_doll(
        &mut self,
        gpu: &Gpu,
        target: &wgpu::TextureView,
        view_proj: Mat4,
        entities: &[EntityDraw],
        characters: &[CharacterDraw],
        doll: Option<(crate::ui::Rect, Mat4, &[CharacterDraw])>,
    ) {
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            params: [self.lightmap_scale, 0.0, 0.0, 0.0],
        };
        gpu.queue
            .write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
        if let Some((_, doll_vp, _)) = &doll {
            let g = Globals {
                view_proj: doll_vp.to_cols_array_2d(),
                params: [self.lightmap_scale, 0.0, 0.0, 0.0],
            };
            gpu.queue
                .write_buffer(&self.doll_globals_buf, 0, bytemuck::bytes_of(&g));
        }
        self.entity_vertices.clear();
        for e in entities {
            box_vertices(&mut self.entity_vertices, e);
        }
        if self.entity_vertices.len() > self.entity_capacity {
            self.entity_capacity = self.entity_vertices.len().next_power_of_two();
            self.entity_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("entity vertices"),
                size: (self.entity_capacity * std::mem::size_of::<EntityVertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !self.entity_vertices.is_empty() {
            gpu.queue.write_buffer(
                &self.entity_buf,
                0,
                bytemuck::cast_slice(&self.entity_vertices),
            );
        }
        // The frame's effects, uploaded and forgotten: the next frame makes its own.
        if self.fx.verts.len() > self.fx_capacity {
            self.fx_capacity = self.fx.verts.len().next_power_of_two();
            self.fx_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("effect vertices"),
                size: (self.fx_capacity * std::mem::size_of::<crate::fx::FxVertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        self.fx_drawn = self.fx.verts.len() as u32;
        if self.fx_drawn > 0 {
            gpu.queue
                .write_buffer(&self.fx_buf, 0, bytemuck::cast_slice(&self.fx.verts));
        }
        self.fx.clear();
        self.characters
            .prepare_with_dolls(gpu, characters, doll.map_or(&[][..], |d| d.2));
        self.hud.prepare(gpu);
        self.draw_calls = (self.world.index_count > 0) as usize
            + (self.fx_drawn > 0) as usize
            + !self.entity_vertices.is_empty() as usize
            + self.characters.drawn
            + !self.hud.is_empty() as usize;
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("world"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.02,
                            b: 0.03,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if self.world.index_count > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.globals_bg, &[]);
                pass.set_bind_group(1, &self.world.textures_bg, &[]);
                pass.set_vertex_buffer(0, self.world.vertex_buf.slice(..));
                pass.set_index_buffer(self.world.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.world.index_count, 0, 0..1);
            }
            self.characters.draw(&mut pass, &self.globals_bg);
            if !self.entity_vertices.is_empty() {
                pass.set_pipeline(&self.entity_pipeline);
                pass.set_bind_group(0, &self.globals_bg, &[]);
                pass.set_vertex_buffer(0, self.entity_buf.slice(..));
                pass.draw(0..self.entity_vertices.len() as u32, 0..1);
            }
            if self.fx_drawn > 0 {
                pass.set_pipeline(&self.fx_pipeline);
                pass.set_bind_group(0, &self.globals_bg, &[]);
                pass.set_vertex_buffer(0, self.fx_buf.slice(..));
                pass.draw(0..self.fx_drawn, 0..1);
            }
            match &doll {
                // The plates, then the doll (its own pass), then the rest.
                Some(_) => self.hud.draw_layers(&mut pass, 0, 0),
                None => self.hud.draw(&mut pass),
            }
        }
        if let Some((rect, _, draws)) = &doll
            && !draws.is_empty()
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("paperdoll"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let (fw, fh) = (self.depth_size.0 as f32, self.depth_size.1 as f32);
            let x = rect.x.clamp(0.0, fw - 1.0).round();
            let y = rect.y.clamp(0.0, fh - 1.0).round();
            let w = rect.w.min(fw - x).max(1.0).round();
            let h = rect.h.min(fh - y).max(1.0).round();
            pass.set_viewport(x, y, w, h, 0.0, 1.0);
            pass.set_scissor_rect(x as u32, y as u32, w as u32, h as u32);
            self.characters.draw_dolls(&mut pass, &self.doll_globals_bg);
            // Back to the whole frame for the HUD's ink (LOOK.md 5).
            pass.set_viewport(0.0, 0.0, fw, fh, 0.0, 1.0);
            pass.set_scissor_rect(0, 0, self.depth_size.0, self.depth_size.1);
            self.hud.draw_layers(&mut pass, 1, crate::hud::LAYERS - 1);
        } else if doll.is_some() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("hud ink"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.hud.draw_layers(&mut pass, 1, crate::hud::LAYERS - 1);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

#[cfg(test)]
mod tests {
    use super::array_layers;

    #[test]
    fn a_texture_array_never_has_a_count_the_gl_backend_takes_for_something_else() {
        for textures in 0..200usize {
            let layers = array_layers(textures);
            assert!(
                layers as usize >= textures,
                "{textures}: room for every texture"
            );
            assert!(
                layers >= 2,
                "{textures}: one layer is a plain texture there"
            );
            assert!(
                !layers.is_multiple_of(6),
                "{textures}: {layers} is a cube's count"
            );
            assert!(
                layers as usize <= textures.max(2) + 1,
                "{textures}: at most one spare"
            );
        }
        // The arena's seven stay seven; the town's twelve become thirteen.
        assert_eq!((array_layers(7), array_layers(12)), (7, 13));
    }
}
