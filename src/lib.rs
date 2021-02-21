//! A render backend to use [egui](https://github.com/emilk/egui) with [wgpu](https://github.com/gfx-rs/wgpu-rs).
//!
//! You need to create a [`RenderPass`] and feed it with the output data provided by egui.
//! A basic usage example can be found [here](https://github.com/hasenbanck/egui_example).
#![warn(missing_docs)]

use bytemuck::{Pod, Zeroable};
pub use epi;
pub use epi::egui;
pub use wgpu;
use wgpu::{include_spirv, util::DeviceExt};

/// Enum for selecting the right buffer type.
#[derive(Debug)]
enum BufferType {
    Uniform,
    Index,
    Vertex,
}

/// Information about the screen used for rendering.
pub struct ScreenDescriptor {
    /// Width of the window in physical pixel.
    pub physical_width: u32,
    /// Height of the window in physical pixel.
    pub physical_height: u32,
    /// HiDPI scale factor.
    pub scale_factor: f32,
}

impl ScreenDescriptor {
    fn logical_size(&self) -> (u32, u32) {
        let logical_width = self.physical_width as f32 / self.scale_factor;
        let logical_height = self.physical_height as f32 / self.scale_factor;
        (logical_width as u32, logical_height as u32)
    }
}

/// Uniform buffer used when rendering.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct UniformBuffer {
    screen_size: [f32; 2],
}

unsafe impl Pod for UniformBuffer {}

unsafe impl Zeroable for UniformBuffer {}

/// Wraps the buffers and includes additional information.
#[derive(Debug)]
struct SizedBuffer {
    buffer: wgpu::Buffer,
    size: usize,
}

/// RenderPass to render a egui based GUI.
pub struct RenderPass {
    render_pipeline: wgpu::RenderPipeline,
    index_buffers: Vec<SizedBuffer>,
    vertex_buffers: Vec<SizedBuffer>,
    uniform_buffer: SizedBuffer,
    uniform_bind_group: wgpu::BindGroup,
    texture_bind_group_layout: wgpu::BindGroupLayout,
    texture_bind_group: Option<wgpu::BindGroup>,
    texture_version: Option<u64>,
    next_user_texture_id: u64,
    pending_user_textures: Vec<(u64, egui::Texture)>,
    user_textures: Vec<Option<wgpu::BindGroup>>,
}

impl RenderPass {
    /// Creates a new render pass to render a egui UI. `output_format` needs to be either `wgpu::TextureFormat::Rgba8UnormSrgb` or `wgpu::TextureFormat::Bgra8UnormSrgb`. Panics if it's not a Srgb format.
    pub fn new(device: &wgpu::Device, output_format: wgpu::TextureFormat) -> Self {
        if !(output_format == wgpu::TextureFormat::Rgba8UnormSrgb
            || output_format == wgpu::TextureFormat::Bgra8UnormSrgb)
        {
            panic!("Incompatible output_format. Needs to be either Rgba8UnormSrgb or Bgra8UnormSrgb: {:?}", output_format);
        }

        let vs_module = device.create_shader_module(&include_spirv!("shader/egui.vert.spirv"));
        let fs_module = device.create_shader_module(&include_spirv!("shader/egui.frag.spirv"));

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("egui_uniform_buffer"),
            contents: bytemuck::cast_slice(&[UniformBuffer {
                screen_size: [0.0, 0.0],
            }]),
            usage: wgpu::BufferUsage::UNIFORM | wgpu::BufferUsage::COPY_DST,
        });
        let uniform_buffer = SizedBuffer {
            buffer: uniform_buffer,
            size: std::mem::size_of::<UniformBuffer>(),
        };

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("egui_texture_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("egui_uniform_bind_group_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStage::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            has_dynamic_offset: false,
                            min_binding_size: None,
                            ty: wgpu::BufferBindingType::Uniform,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStage::FRAGMENT,
                        ty: wgpu::BindingType::Sampler {
                            filtering: true,
                            comparison: false,
                        },
                        count: None,
                    },
                ],
            });

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("egui_uniform_bind_group"),
            layout: &uniform_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer {
                        buffer: &uniform_buffer.buffer,
                        offset: 0,
                        size: None,
                    },
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("egui_texture_bind_group_layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStage::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                }],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("egui_pipeline_layout"),
            bind_group_layouts: &[&uniform_bind_group_layout, &texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("egui_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                entry_point: "main",
                module: &vs_module,
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 5 * 4,
                    step_mode: wgpu::InputStepMode::Vertex,
                    // 0: vec2 position
                    // 1: vec2 texture coordinates
                    // 2: uint color
                    attributes: &wgpu::vertex_attr_array![0 => Float2, 1 => Float2, 2 => Uint],
                }],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                front_face: wgpu::FrontFace::default(),
                polygon_mode: wgpu::PolygonMode::default(),
                strip_index_format: Some(wgpu::IndexFormat::Uint32),
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                alpha_to_coverage_enabled: false,
                count: 1,
                mask: !0,
            },
            fragment: Some(wgpu::FragmentState {
                module: &fs_module,
                entry_point: "main",
                targets: &[wgpu::ColorTargetState {
                    format: output_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                        alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::OneMinusDstAlpha,
                        dst_factor: wgpu::BlendFactor::One,
                        operation: wgpu::BlendOperation::Add,
                        }
                    }),
                    write_mask: wgpu::ColorWrite::ALL,
                }],
            }),
        });

        Self {
            render_pipeline,
            vertex_buffers: Vec::with_capacity(64),
            index_buffers: Vec::with_capacity(64),
            uniform_buffer,
            uniform_bind_group,
            texture_bind_group_layout,
            texture_version: None,
            texture_bind_group: None,
            next_user_texture_id: 0,
            pending_user_textures: Vec::new(),
            user_textures: Vec::new(),
        }
    }

    /// Executes the egui render pass. When `clear_on_draw` is set, the output target will get cleared before writing to it.
    pub fn execute(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        color_attachment: &wgpu::TextureView,
        paint_jobs: &[egui::paint::ClippedMesh],
        screen_descriptor: &ScreenDescriptor,
        clear_color: Option<wgpu::Color>,
    ) {
        let load_operation = if let Some(color) = clear_color {
            wgpu::LoadOp::Clear(color)
        } else {
            wgpu::LoadOp::Load
        };

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[wgpu::RenderPassColorAttachmentDescriptor {
                attachment: color_attachment,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: load_operation,
                    store: true,
                },
            }],
            depth_stencil_attachment: None,
            label: Some("egui main render pass"),
        });
        pass.push_debug_group("egui_pass");
        pass.set_pipeline(&self.render_pipeline);

        pass.set_bind_group(0, &self.uniform_bind_group, &[]);

        let scale_factor = screen_descriptor.scale_factor;
        let physical_width = screen_descriptor.physical_width;
        let physical_height = screen_descriptor.physical_height;

        for ((egui::ClippedMesh(clip_rect, mesh), vertex_buffer), index_buffer) in paint_jobs
            .iter()
            .zip(self.vertex_buffers.iter())
            .zip(self.index_buffers.iter())
        {
            // Transform clip rect to physical pixels.
            let clip_min_x = scale_factor * clip_rect.min.x;
            let clip_min_y = scale_factor * clip_rect.min.y;
            let clip_max_x = scale_factor * clip_rect.max.x;
            let clip_max_y = scale_factor * clip_rect.max.y;

            // Make sure clip rect can fit within an `u32`.
            let clip_min_x = egui::clamp(clip_min_x, 0.0..=physical_width as f32);
            let clip_min_y = egui::clamp(clip_min_y, 0.0..=physical_height as f32);
            let clip_max_x = egui::clamp(clip_max_x, clip_min_x..=physical_width as f32);
            let clip_max_y = egui::clamp(clip_max_y, clip_min_y..=physical_height as f32);

            let clip_min_x = clip_min_x.round() as u32;
            let clip_min_y = clip_min_y.round() as u32;
            let clip_max_x = clip_max_x.round() as u32;
            let clip_max_y = clip_max_y.round() as u32;

            let width = (clip_max_x - clip_min_x).max(1);
            let height = (clip_max_y - clip_min_y).max(1);

            {
                // clip scissor rectangle to target size
                let x = clip_min_x.min(physical_width);
                let y = clip_min_y.min(physical_height);
                let width = width.min(physical_width - x);
                let height = height.min(physical_height - y);

                // skip rendering with zero-sized clip areas
                if width == 0 || height == 0 {
                    continue;
                }

                pass.set_scissor_rect(x, y, width, height);
            }
            pass.set_bind_group(1, self.get_texture_bind_group(mesh.texture_id), &[]);

            pass.set_index_buffer(index_buffer.buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.set_vertex_buffer(0, vertex_buffer.buffer.slice(..));
            pass.draw_indexed(0..mesh.indices.len() as u32, 0, 0..1);
        }

        pass.pop_debug_group();
    }

    fn get_texture_bind_group(&self, texture_id: egui::TextureId) -> &wgpu::BindGroup {
        match texture_id {
            egui::TextureId::Egui => self
                .texture_bind_group
                .as_ref()
                .expect("egui texture was not set before the first draw"),
            egui::TextureId::User(id) => {
                let id = id as usize;
                assert!(id < self.user_textures.len());
                &(self
                    .user_textures
                    .get(id)
                    .unwrap_or_else(|| panic!("user texture {} not found", id))
                    .as_ref()
                    .unwrap_or_else(|| panic!("user texture {} freed", id)))
            }
        }
    }

    /// Updates the texture used by egui for the fonts etc. Should be called before `execute()`.
    pub fn update_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        egui_texture: &egui::Texture,
    ) {
        // Don't update the texture if it hasn't changed.
        if self.texture_version == Some(egui_texture.version) {
            return;
        }
        // we need to convert the texture into rgba_srgb format
        let mut pixels: Vec<u8> = Vec::with_capacity(egui_texture.pixels.len() * 4);
        for srgba in egui_texture.srgba_pixels() {
            pixels.push(srgba.r());
            pixels.push(srgba.g());
            pixels.push(srgba.b());
            pixels.push(srgba.a());
        }
        let egui_texture = egui::Texture {
            version: egui_texture.version,
            width: egui_texture.width,
            height: egui_texture.height,
            pixels,
        };
        let bind_group = self.egui_texture_to_wgpu(device, queue, &egui_texture, "egui");

        self.texture_version = Some(egui_texture.version);
        self.texture_bind_group = Some(bind_group);
    }

    /// Updates the user textures that the app allocated. Should be called before `execute()`.
    pub fn update_user_textures(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let pending_user_textures = std::mem::take(&mut self.pending_user_textures);
        for (id, texture) in pending_user_textures {
            let bind_group = self.egui_texture_to_wgpu(
                device,
                queue,
                &texture,
                format!("user_texture{}", id).as_str(),
            );
            self.user_textures.push(Some(bind_group));
        }
    }

    // Assumes egui_texture contains srgb data.
    // This does not match how egui::Texture is documented as of writing, but this is how it is used for user textures.
    fn egui_texture_to_wgpu(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        egui_texture: &egui::Texture,
        label: &str,
    ) -> wgpu::BindGroup {
        let size = wgpu::Extent3d {
            width: egui_texture.width as u32,
            height: egui_texture.height as u32,
            depth: 1,
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(format!("{}_texture", label).as_str()),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsage::SAMPLED | wgpu::TextureUsage::COPY_DST,
        });

        queue.write_texture(
            wgpu::TextureCopyView {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
            },
            egui_texture.pixels.as_slice(),
            wgpu::TextureDataLayout {
                offset: 0,
                bytes_per_row: (egui_texture.pixels.len() / egui_texture.height) as u32,
                rows_per_image: egui_texture.height as u32,
            },
            size,
        );

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(format!("{}_texture_bind_group", label).as_str()),
            layout: &self.texture_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(
                    &texture.create_view(&wgpu::TextureViewDescriptor::default()),
                ),
            }],
        });

        bind_group
    }

    /// Registers a `wgpu::Texture` with a `egui::TextureId`.
    ///
    /// This enables the application to reference
    /// the texture inside an image ui element. This effectively enables off-screen rendering inside
    /// the egui UI. Texture must have the texture format `TextureFormat::Rgba8UnormSrgb` and 
    /// Texture usage `TextureUsage::SAMPLED`.
    pub fn egui_texture_from_wgpu_texture(
        &mut self,
        device: &wgpu::Device,
        texture: &wgpu::Texture,
    ) -> egui::TextureId {

        // We have to bind it here, so that we don't add it as a pending texture.
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(format!("{}_texture_bind_group", self.next_user_texture_id).as_str()),
            layout: &self.texture_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(
                    &texture.create_view(&wgpu::TextureViewDescriptor::default()),
                ),
            }],
        });
        let texture_id = egui::TextureId::User(self.next_user_texture_id);
        self.user_textures.push(Some(bind_group));
        self.next_user_texture_id += 1;

        texture_id
    }

    /// Uploads the uniform, vertex and index data used by the render pass. Should be called before `execute()`.
    pub fn update_buffers(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        paint_jobs: &[egui::paint::ClippedMesh],
        screen_descriptor: &ScreenDescriptor,
    ) {
        let index_size = self.index_buffers.len();
        let vertex_size = self.vertex_buffers.len();

        let (logical_width, logical_height) = screen_descriptor.logical_size();

        self.update_buffer(
            device,
            queue,
            BufferType::Uniform,
            0,
            bytemuck::cast_slice(&[UniformBuffer {
                screen_size: [logical_width as f32, logical_height as f32],
            }]),
        );

        for (i, egui::ClippedMesh(_, mesh)) in paint_jobs.iter().enumerate() {
            let data: &[u8] = bytemuck::cast_slice(&mesh.indices);
            if i < index_size {
                self.update_buffer(device, queue, BufferType::Index, i, data)
            } else {
                let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("egui_index_buffer"),
                    contents: data,
                    usage: wgpu::BufferUsage::INDEX | wgpu::BufferUsage::COPY_DST,
                });
                self.index_buffers.push(SizedBuffer {
                    buffer,
                    size: data.len(),
                });
            }

            let data: &[u8] = as_byte_slice(&mesh.vertices);
            if i < vertex_size {
                self.update_buffer(device, queue, BufferType::Vertex, i, data)
            } else {
                let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("egui_vertex_buffer"),
                    contents: data,
                    usage: wgpu::BufferUsage::VERTEX | wgpu::BufferUsage::COPY_DST,
                });

                self.vertex_buffers.push(SizedBuffer {
                    buffer,
                    size: data.len(),
                });
            }
        }
    }

    /// Updates the buffers used by egui. Will properly re-size the buffers if needed.
    fn update_buffer(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        buffer_type: BufferType,
        index: usize,
        data: &[u8],
    ) {
        let (buffer, storage, name) = match buffer_type {
            BufferType::Index => (
                &mut self.index_buffers[index],
                wgpu::BufferUsage::INDEX,
                "index",
            ),
            BufferType::Vertex => (
                &mut self.vertex_buffers[index],
                wgpu::BufferUsage::VERTEX,
                "vertex",
            ),
            BufferType::Uniform => (
                &mut self.uniform_buffer,
                wgpu::BufferUsage::UNIFORM,
                "uniform",
            ),
        };

        if data.len() > buffer.size {
            buffer.size = data.len();
            buffer.buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(format!("egui_{}_buffer", name).as_str()),
                contents: bytemuck::cast_slice(data),
                usage: storage | wgpu::BufferUsage::COPY_DST,
            });
        } else {
            queue.write_buffer(&buffer.buffer, 0, data);
        }
    }
}

impl epi::TextureAllocator for RenderPass {
    fn alloc_srgba_premultiplied(
        &mut self,
        size: (usize, usize),
        srgba_pixels: &[egui::Color32],
    ) -> egui::TextureId {
        let id = self.next_user_texture_id;
        self.next_user_texture_id += 1;

        let mut pixels = vec![0u8; srgba_pixels.len() * 4];
        for (target, given) in pixels.chunks_exact_mut(4).zip(srgba_pixels.iter()) {
            target.copy_from_slice(&given.to_array());
        }

        let (width, height) = size;
        self.pending_user_textures.push((
            id,
            egui::Texture {
                version: 0,
                width,
                height,
                pixels,
            },
        ));

        egui::TextureId::User(id)
    }

    fn free(&mut self, id: egui::TextureId) {
        if let egui::TextureId::User(id) = id {
            self.user_textures
                .get_mut(id as usize)
                .and_then(|option| option.take());
        }
    }
}

// Needed since we can't use bytemuck for external types.
fn as_byte_slice<T>(slice: &[T]) -> &[u8] {
    let len = slice.len() * std::mem::size_of::<T>();
    let ptr = slice.as_ptr() as *const u8;
    unsafe { std::slice::from_raw_parts(ptr, len) }
}
