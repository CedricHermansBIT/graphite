#[cfg(not(feature = "gpu"))]
pub struct GpuRepulsion;

#[cfg(not(feature = "gpu"))]
impl GpuRepulsion {
    pub fn evaluate(
        &self,
        _nodes: &[crate::rust_layout::QuadNode],
        _ordered: &[crate::rust_layout::MortonPoint],
        _positions: &[crate::rust_layout::Pos2],
        _theta: f32,
        _output: &mut [crate::rust_layout::Pos2],
    ) -> Result<(), &'static str> {
        Err("GPU feature is disabled")
    }
}

#[cfg(feature = "gpu")]
mod enabled {
    use crate::rust_layout::{MortonPoint, Pos2, QuadNode};
    use anyhow::Context;
    use bytemuck::{Pod, Zeroable};
    use wgpu::util::DeviceExt;

    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable)]
    struct GpuNode {
        bounds: [f32; 4],
        mass: [f32; 4],
        range: [u32; 4],
        children: [i32; 4],
    }

    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable)]
    struct Params {
        count: u32,
        theta: f32,
        padding: [u32; 2],
    }

    pub enum GpuRepulsion {
        #[cfg(feature = "cuda")]
        Cuda(crate::cuda_layout::CudaRepulsion),
        Wgpu(WgpuRepulsion),
    }

    pub struct WgpuRepulsion {
        device: wgpu::Device,
        queue: wgpu::Queue,
        pipeline: wgpu::ComputePipeline,
    }

    impl GpuRepulsion {
        pub fn new() -> anyhow::Result<Self> {
            #[cfg(feature = "cuda")]
            let cuda_error = match crate::cuda_layout::CudaRepulsion::new() {
                Ok(cuda) => return Ok(Self::Cuda(cuda)),
                Err(error) => error,
            };
            #[cfg(feature = "cuda")]
            {
                WgpuRepulsion::new().map(Self::Wgpu).map_err(|wgpu_error| {
                    anyhow::anyhow!(
                        "no usable GPU compute backend; CUDA: {cuda_error:#}; wgpu: {wgpu_error:#}"
                    )
                })
            }
            #[cfg(not(feature = "cuda"))]
            {
                WgpuRepulsion::new().map(Self::Wgpu)
            }
        }

        pub fn evaluate(
            &self,
            nodes: &[QuadNode],
            ordered: &[MortonPoint],
            positions: &[Pos2],
            theta: f32,
            output: &mut [Pos2],
        ) -> Result<(), &'static str> {
            match self {
                #[cfg(feature = "cuda")]
                Self::Cuda(cuda) => cuda.evaluate(nodes, ordered, positions, theta, output),
                Self::Wgpu(wgpu) => wgpu.evaluate(nodes, ordered, positions, theta, output),
            }
        }
    }

    impl WgpuRepulsion {
        fn new() -> anyhow::Result<Self> {
            let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
            descriptor.backends = wgpu::Backends::PRIMARY;
            let instance = wgpu::Instance::new(descriptor);
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                    ..Default::default()
                }))
                .context("no compatible GPU compute adapter was found")?;
            let adapter_info = adapter.get_info();
            if adapter_info.device_type == wgpu::DeviceType::Cpu
                && std::env::var_os("GRAPHITE_ALLOW_SOFTWARE_GPU").is_none()
            {
                anyhow::bail!(
                    "only a software Vulkan adapter was found; set GRAPHITE_ALLOW_SOFTWARE_GPU=1 to test it explicitly"
                );
            }
            let adapter_name = adapter_info.name;
            log::info!(
                "GPU layout selected {} via {:?}",
                adapter_name,
                adapter_info.backend
            );
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("Graphite layout compute"),
                    required_features: wgpu::Features::empty(),
                    required_limits: adapter.limits(),
                    ..Default::default()
                }))
                .with_context(|| {
                    format!(
                        "could not open GPU compute device ({adapter_name}, {:?})",
                        adapter_info.backend
                    )
                })?;
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Graphite Barnes-Hut repulsion"),
                source: wgpu::ShaderSource::Wgsl(include_str!("gpu_repulsion.wgsl").into()),
            });
            let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("Graphite Barnes-Hut repulsion"),
                layout: None,
                module: &shader,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            });
            Ok(Self {
                device,
                queue,
                pipeline,
            })
        }

        pub fn evaluate(
            &self,
            nodes: &[QuadNode],
            ordered: &[MortonPoint],
            positions: &[Pos2],
            theta: f32,
            output: &mut [Pos2],
        ) -> Result<(), &'static str> {
            if positions.is_empty() || output.len() != positions.len() {
                return Err("GPU layout buffer lengths are invalid");
            }
            let limits = self.device.limits();
            let max_bytes = limits.max_storage_buffer_binding_size as usize;
            if positions.len() > u32::MAX as usize
                || positions.len().div_ceil(64)
                    > limits.max_compute_workgroups_per_dimension as usize
                || nodes.len() * std::mem::size_of::<GpuNode>() > max_bytes
                || positions.len() * std::mem::size_of::<Pos2>() > max_bytes
                || ordered.len() * std::mem::size_of::<u32>() > max_bytes
            {
                return Err("layout exceeds this GPU's compute buffer limits");
            }
            let gpu_nodes: Vec<GpuNode> = nodes
                .iter()
                .map(|node| GpuNode {
                    bounds: [node.center[0], node.center[1], node.half, node.charge],
                    mass: [node.center_of_charge[0], node.center_of_charge[1], 0.0, 0.0],
                    range: [node.first, node.end, 0, 0],
                    children: node.children,
                })
                .collect();
            let order: Vec<u32> = ordered.iter().map(|point| point.index).collect();
            let node_buffer = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("quadtree"),
                    contents: bytemuck::cast_slice(&gpu_nodes),
                    usage: wgpu::BufferUsages::STORAGE,
                });
            let order_buffer = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("particle order"),
                    contents: bytemuck::cast_slice(&order),
                    usage: wgpu::BufferUsages::STORAGE,
                });
            let position_buffer =
                self.device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("positions"),
                        contents: bytemuck::cast_slice(positions),
                        usage: wgpu::BufferUsages::STORAGE,
                    });
            let params = Params {
                count: positions.len() as u32,
                theta,
                padding: [0; 2],
            };
            let params_buffer = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("repulsion parameters"),
                    contents: bytemuck::bytes_of(&params),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
            let byte_size = std::mem::size_of_val(output) as u64;
            let result_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("repulsion output"),
                size: byte_size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("repulsion readback"),
                size: byte_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("repulsion bindings"),
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: [
                    (0, &node_buffer),
                    (1, &order_buffer),
                    (2, &position_buffer),
                    (3, &params_buffer),
                    (4, &result_buffer),
                ]
                .map(|(binding, buffer)| wgpu::BindGroupEntry {
                    binding,
                    resource: buffer.as_entire_binding(),
                })
                .as_slice(),
            });
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("repulsion encoder"),
                });
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("Barnes-Hut traversal"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.dispatch_workgroups((positions.len() as u32).div_ceil(64), 1, 1);
            }
            encoder.copy_buffer_to_buffer(&result_buffer, 0, &readback, 0, byte_size);
            self.queue.submit([encoder.finish()]);
            let (sender, receiver) = std::sync::mpsc::channel();
            readback.map_async(wgpu::MapMode::Read, .., move |result| {
                let _ = sender.send(result);
            });
            self.device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(|_| "GPU submission failed")?;
            receiver
                .recv()
                .map_err(|_| "GPU readback did not complete")?
                .map_err(|_| "GPU readback failed")?;
            {
                let bytes = readback
                    .get_mapped_range(..)
                    .map_err(|_| "GPU readback mapping failed")?;
                output.copy_from_slice(bytemuck::cast_slice(&bytes));
            }
            readback.unmap();
            Ok(())
        }
    }
}

#[cfg(feature = "gpu")]
pub use enabled::GpuRepulsion;
