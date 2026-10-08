//! Skinned characters (MODELS.md 9): one draw call each, one atlas each, 24 skinning matrices
//! in a uniform block picked by a dynamic offset. Models and the mannequins share everything
//! but their buffers and texture.

use glam::{Mat4, Vec3};
use gm_model::format::{Vertex, bc1_level_bytes, quantize_vertices};
use gm_model::mannequin::MeshData;
use gm_model::{BONES, Model, Pose, bc1, skin_matrices};

use crate::render::{DEPTH_FORMAT, Gpu};

/// One character this frame.
#[derive(Clone, Copy, Debug)]
pub struct CharacterDraw {
    /// A slot returned by `add_model` or `add_mesh`.
    pub slot: usize,
    /// Model space (feet at the origin, facing +X) to the world.
    pub world: Mat4,
    pub pose: Pose,
    /// Multiplies the texture: the armour tint of a mannequin, white for a model.
    pub tint: [f32; 3],
    /// Ambient light where the character stands.
    pub light: [f32; 3],
    /// A prop (LOOK.md 6.3): not posed, but placed by this one matrix (its model space to
    /// the wearer's), which goes to bone 0 of the block; the other bones are unused.
    pub attach: Option<Mat4>,
}

/// What the animation and the caches need to know about a loaded model.
#[derive(Clone, Copy, Debug)]
pub struct ModelInfo {
    pub pivots: [Vec3; BONES],
    pub mask: u32,
    pub triangles: u32,
    pub gpu_bytes: usize,
}

impl ModelInfo {
    pub fn hips_z(&self) -> f32 {
        self.pivots[gm_model::rig::bone::HIPS].z
    }
}

struct GpuModel {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
    texture: wgpu::BindGroup,
    scale: [f32; 3],
    two_sided: bool,
    info: ModelInfo,
}

/// The uniform block of one character.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Block {
    bones: [[[f32; 4]; 4]; BONES],
    scale: [f32; 4],
    tint: [f32; 4],
    light: [f32; 4],
}

const BLOCK_BYTES: u64 = std::mem::size_of::<Block>() as u64;

const SHADER: &str = r#"
struct Globals { view_proj: mat4x4<f32>, params: vec4<f32> };
struct Character {
    bones: array<mat4x4<f32>, 24>,
    scale: vec4<f32>,
    tint: vec4<f32>,
    light: vec4<f32>,
};
@group(0) @binding(0) var<uniform> globals: Globals;
@group(1) @binding(0) var<uniform> character: Character;
@group(2) @binding(0) var atlas: texture_2d<f32>;
@group(2) @binding(1) var atlas_sampler: sampler;

struct VsIn {
    @location(0) pos: vec4<f32>,
    @location(1) normal: vec4<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) joints: vec4<u32>,
    @location(4) weights: vec4<f32>,
};
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) shade: vec3<f32>,
};

@vertex fn vs_main(in: VsIn) -> VsOut {
    let skin = character.bones[in.joints.x] * in.weights.x
        + character.bones[in.joints.y] * in.weights.y
        + character.bones[in.joints.z] * in.weights.z
        + character.bones[in.joints.w] * in.weights.w;
    let world = skin * vec4<f32>(in.pos.xyz * character.scale.xyz, 1.0);
    let n = normalize((skin * vec4<f32>(in.normal.xyz, 0.0)).xyz);
    // Half-lambert against a fixed key light: shape without a lighting pass.
    let key = normalize(vec3<f32>(0.35, 0.25, 0.9));
    let wrap = 0.5 + 0.5 * dot(n, key);
    var out: VsOut;
    out.clip = globals.view_proj * world;
    out.uv = in.uv;
    out.shade = character.tint.rgb * character.light.rgb * (0.45 + 0.55 * wrap * wrap);
    return out;
}

@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let albedo = textureSample(atlas, atlas_sampler, in.uv);
    if (albedo.a < 0.5) { discard; }
    return vec4<f32>(albedo.rgb * in.shade, 1.0);
}
"#;

pub struct Characters {
    pipeline: wgpu::RenderPipeline,
    pipeline_two_sided: wgpu::RenderPipeline,
    block_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    blocks: wgpu::Buffer,
    blocks_bind: wgpu::BindGroup,
    /// Characters the block buffer holds.
    capacity: usize,
    /// Bytes between two blocks (the block size rounded up to the device's alignment).
    stride: u64,
    staging: Vec<u8>,
    /// `(slot, block index)` in draw order.
    order: Vec<(usize, usize)>,
    /// The paperdoll's draws (LOOK.md 5), after the world's in the block buffer.
    doll_order: Vec<(usize, usize)>,
    models: Vec<Option<GpuModel>>,
    free: Vec<usize>,
    gpu_bytes: usize,
    pub drawn: usize,
    pub triangles: usize,
}

fn block_buffer(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    stride: u64,
    capacity: usize,
) -> (wgpu::Buffer, wgpu::BindGroup) {
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("character blocks"),
        size: stride * capacity as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("character blocks"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: &buf,
                offset: 0,
                size: wgpu::BufferSize::new(BLOCK_BYTES),
            }),
        }],
    });
    (buf, bind)
}

impl Characters {
    pub fn new(
        gpu: &Gpu,
        color_format: wgpu::TextureFormat,
        globals_layout: &wgpu::BindGroupLayout,
    ) -> Characters {
        let device = &gpu.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("characters"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let block_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("character block"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(BLOCK_BYTES),
                },
                count: None,
            }],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("character atlas"),
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
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("characters"),
            bind_group_layouts: &[
                Some(globals_layout),
                Some(&block_layout),
                Some(&texture_layout),
            ],
            immediate_size: 0,
        });
        let pipeline = |cull: Option<wgpu::Face>, label: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Vertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![
                            0 => Snorm16x4, 1 => Snorm8x4, 2 => Unorm16x2, 3 => Uint8x4, 4 => Unorm8x4
                        ],
                    })],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: cull,
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
            })
        };
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("character atlas"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            lod_min_clamp: 0.0,
            lod_max_clamp: 32.0,
            compare: None,
            anisotropy_clamp: 1,
            border_color: None,
        });
        let align = device.limits().min_uniform_buffer_offset_alignment.max(1) as u64;
        let stride = BLOCK_BYTES.div_ceil(align) * align;
        let capacity = 64;
        let (blocks, blocks_bind) = block_buffer(device, &block_layout, stride, capacity);
        Characters {
            pipeline: pipeline(Some(wgpu::Face::Back), "characters"),
            pipeline_two_sided: pipeline(None, "characters two-sided"),
            block_layout,
            texture_layout,
            sampler,
            blocks,
            blocks_bind,
            capacity,
            stride,
            staging: Vec::new(),
            order: Vec::new(),
            doll_order: Vec::new(),
            models: Vec::new(),
            free: Vec::new(),
            gpu_bytes: 0,
            drawn: 0,
            triangles: 0,
        }
    }

    fn insert(&mut self, model: GpuModel) -> usize {
        self.gpu_bytes += model.info.gpu_bytes;
        match self.free.pop() {
            Some(slot) => {
                self.models[slot] = Some(model);
                slot
            }
            None => {
                self.models.push(Some(model));
                self.models.len() - 1
            }
        }
    }

    fn texture_bind(&self, device: &wgpu::Device, texture: &wgpu::Texture) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("character atlas"),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &texture.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    /// Upload an ingested model; returns its slot.
    pub fn add_model(&mut self, gpu: &Gpu, model: &Model) -> usize {
        let device = &gpu.device;
        let levels: Vec<(u32, u32, &[u8])> = model.mips().collect();
        let texture = if gpu.bc {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("model atlas"),
                size: wgpu::Extent3d {
                    width: model.tex_w as u32,
                    height: model.tex_h as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: levels.len() as u32,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bc1RgbaUnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            for (i, (w, h, blocks)) in levels.iter().enumerate() {
                gpu.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: i as u32,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    blocks,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(w.div_ceil(4) * 8),
                        rows_per_image: Some(h.div_ceil(4)),
                    },
                    wgpu::Extent3d {
                        width: *w,
                        height: *h,
                        depth_or_array_layers: 1,
                    },
                );
            }
            texture
        } else {
            // No BC on this GPU: decode on the CPU and leave out the largest level, which is
            // three quarters of the memory (MODELS.md 8).
            let skip = usize::from(levels.len() > 1);
            let kept = &levels[skip..];
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("model atlas (decoded)"),
                size: wgpu::Extent3d {
                    width: kept[0].0,
                    height: kept[0].1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: kept.len() as u32,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            for (i, (w, h, blocks)) in kept.iter().enumerate() {
                debug_assert_eq!(blocks.len(), bc1_level_bytes(*w as usize, *h as usize));
                let rgba = bc1::decode(blocks, *w as usize, *h as usize);
                write_rgba(&gpu.queue, &texture, i as u32, *w, *h, &rgba);
            }
            texture
        };
        let gpu_model = GpuModel {
            vertices: gpu.buffer(
                "model vertices",
                bytemuck::cast_slice(&model.vertices),
                wgpu::BufferUsages::VERTEX,
            ),
            indices: gpu.buffer(
                "model indices",
                bytemuck::cast_slice(&model.indices),
                wgpu::BufferUsages::INDEX,
            ),
            index_count: model.indices.len() as u32,
            texture: self.texture_bind(device, &texture),
            scale: model.scale,
            two_sided: model.two_sided(),
            info: ModelInfo {
                pivots: model.pivots.map(Vec3::from),
                mask: model.bone_mask,
                triangles: model.triangles() as u32,
                gpu_bytes: model_gpu_bytes(model, gpu.bc),
            },
        };
        self.insert(gpu_model)
    }

    /// Upload a generated mesh with an RGBA texture (the mannequins); returns its slot.
    pub fn add_mesh(&mut self, gpu: &Gpu, mesh: &MeshData, side: u32, rgba: &[u8]) -> usize {
        let device = &gpu.device;
        let (scale, vertices) = quantize_vertices(
            &mesh.positions,
            &mesh.normals,
            &mesh.uvs,
            &mesh.joints,
            &mesh.weights,
        );
        let indices: Vec<u16> = mesh.indices.iter().map(|i| *i as u16).collect();
        let mips = crate::render::mip_chain(rgba, side, side);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mannequin atlas"),
            size: wgpu::Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: 1,
            },
            mip_level_count: mips.len() as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut texture_bytes = 0;
        for (i, (w, h, data)) in mips.iter().enumerate() {
            texture_bytes += data.len();
            write_rgba(&gpu.queue, &texture, i as u32, *w, *h, data);
        }
        let gpu_model = GpuModel {
            vertices: gpu.buffer(
                "mannequin vertices",
                bytemuck::cast_slice(&vertices),
                wgpu::BufferUsages::VERTEX,
            ),
            indices: gpu.buffer(
                "mannequin indices",
                bytemuck::cast_slice(&indices),
                wgpu::BufferUsages::INDEX,
            ),
            index_count: indices.len() as u32,
            texture: self.texture_bind(device, &texture),
            scale,
            two_sided: false,
            info: ModelInfo {
                pivots: mesh.pivots,
                mask: mesh.bone_mask,
                triangles: mesh.triangles() as u32,
                gpu_bytes: vertices.len() * std::mem::size_of::<Vertex>()
                    + indices.len() * 2
                    + texture_bytes,
            },
        };
        self.insert(gpu_model)
    }

    /// Free a slot; its buffers and texture are released when the GPU is done with them.
    pub fn remove(&mut self, slot: usize) {
        if let Some(m) = self.models.get_mut(slot).and_then(Option::take) {
            self.gpu_bytes -= m.info.gpu_bytes;
            self.free.push(slot);
        }
    }

    pub fn info(&self, slot: usize) -> Option<&ModelInfo> {
        self.models.get(slot)?.as_ref().map(|m| &m.info)
    }

    /// Bytes of vertex, index and texture memory held by loaded models and mannequins.
    pub fn gpu_bytes(&self) -> usize {
        self.gpu_bytes
    }

    /// Compute and upload this frame's skinning blocks: the world's draws, then the
    /// paperdoll's (drawn by `draw_dolls`).
    pub fn prepare_with_dolls(
        &mut self,
        gpu: &Gpu,
        draws: &[CharacterDraw],
        dolls: &[CharacterDraw],
    ) {
        self.order.clear();
        self.doll_order.clear();
        self.staging.clear();
        self.triangles = 0;
        let world_draws = draws.len();
        for (i, d) in draws.iter().chain(dolls).enumerate() {
            let Some(m) = self.models.get(d.slot).and_then(Option::as_ref) else {
                continue;
            };
            let doll = i >= world_draws;
            let mut block = Block {
                bones: [[[0.0; 4]; 4]; BONES],
                scale: [m.scale[0], m.scale[1], m.scale[2], 0.0],
                tint: [d.tint[0], d.tint[1], d.tint[2], 1.0],
                light: [d.light[0], d.light[1], d.light[2], 1.0],
            };
            match d.attach {
                Some(attach) => block.bones[0] = (d.world * attach).to_cols_array_2d(),
                None => {
                    let skin = skin_matrices(&m.info.pivots, m.info.mask, &d.pose);
                    for (out, s) in block.bones.iter_mut().zip(&skin) {
                        *out = (d.world * *s).to_cols_array_2d();
                    }
                }
            }
            let index = self.order.len() + self.doll_order.len();
            self.staging.extend_from_slice(bytemuck::bytes_of(&block));
            self.staging.resize((index + 1) * self.stride as usize, 0);
            if doll {
                self.doll_order.push((d.slot, index));
            } else {
                self.order.push((d.slot, index));
            }
            self.triangles += m.info.triangles as usize;
        }
        self.drawn = self.order.len() + self.doll_order.len();
        if self.drawn == 0 {
            return;
        }
        if self.drawn > self.capacity {
            self.capacity = self.drawn.next_power_of_two();
            (self.blocks, self.blocks_bind) =
                block_buffer(&gpu.device, &self.block_layout, self.stride, self.capacity);
        }
        gpu.queue.write_buffer(&self.blocks, 0, &self.staging);
        // One-sided models first, then by slot: the fewest pipeline and texture changes.
        let models = &self.models;
        self.order
            .sort_by_key(|(slot, _)| (models[*slot].as_ref().is_some_and(|m| m.two_sided), *slot));
    }

    /// Draw what `prepare` set up.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, globals: &wgpu::BindGroup) {
        self.draw_list(pass, globals, &self.order);
    }

    /// Draw the paperdoll's bodies (`prepare_with_dolls`).
    pub fn draw_dolls(&self, pass: &mut wgpu::RenderPass<'_>, globals: &wgpu::BindGroup) {
        self.draw_list(pass, globals, &self.doll_order);
    }

    fn draw_list(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        globals: &wgpu::BindGroup,
        order: &[(usize, usize)],
    ) {
        if order.is_empty() {
            return;
        }
        pass.set_bind_group(0, globals, &[]);
        let mut current: Option<(bool, usize)> = None;
        for &(slot, index) in order {
            let Some(m) = self.models[slot].as_ref() else {
                continue;
            };
            if current.map(|c| c.0) != Some(m.two_sided) {
                pass.set_pipeline(if m.two_sided {
                    &self.pipeline_two_sided
                } else {
                    &self.pipeline
                });
            }
            if current != Some((m.two_sided, slot)) {
                pass.set_bind_group(2, &m.texture, &[]);
                pass.set_vertex_buffer(0, m.vertices.slice(..));
                pass.set_index_buffer(m.indices.slice(..), wgpu::IndexFormat::Uint16);
                current = Some((m.two_sided, slot));
            }
            pass.set_bind_group(1, &self.blocks_bind, &[(index as u64 * self.stride) as u32]);
            pass.draw_indexed(0..m.index_count, 0, 0..1);
        }
    }
}

/// What `add_model` allocates for `model`: its vertices, its indices and the texture as this
/// GPU holds it (BC1, or RGBA8 without the largest level). The model cache makes room for
/// exactly this before it loads a model, so its cap holds on a GPU without BC too.
pub fn model_gpu_bytes(model: &Model, bc: bool) -> usize {
    let texture = if bc {
        model.texture.len()
    } else {
        let skip = usize::from(model.mips().count() > 1);
        model
            .mips()
            .skip(skip)
            .map(|(w, h, _)| w as usize * h as usize * 4)
            .sum()
    };
    std::mem::size_of_val(model.vertices.as_slice()) + model.indices.len() * 2 + texture
}

fn write_rgba(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    level: u32,
    w: u32,
    h: u32,
    rgba: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: level,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}
