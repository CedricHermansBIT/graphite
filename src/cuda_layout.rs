use crate::rust_layout::{MortonPoint, Pos2, QuadNode};
use cudarc::driver::{
    CudaContext, CudaFunction, CudaStream, DeviceRepr, LaunchConfig, PushKernelArg,
};
use cudarc::nvrtc::{CompileOptions, compile_ptx_with_opts};
use std::sync::Arc;

#[repr(C)]
#[derive(Clone, Copy)]
struct CudaNode {
    bounds: [f32; 4],
    mass: [f32; 4],
    range: [u32; 4],
    children: [i32; 4],
}

// The C kernel uses the same four fixed-size arrays in the same order.
unsafe impl DeviceRepr for CudaNode {}

pub struct CudaRepulsion {
    _context: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    function: CudaFunction,
}

impl CudaRepulsion {
    pub fn new() -> anyhow::Result<Self> {
        if !unsafe { cudarc::driver::sys::is_culib_present() } {
            anyhow::bail!("CUDA driver library is unavailable");
        }
        let context = CudaContext::new(0)
            .map_err(|error| anyhow::anyhow!("no usable CUDA device: {error:?}"))?;
        if !unsafe { cudarc::nvrtc::sys::is_culib_present() } {
            anyhow::bail!("CUDA runtime compiler library is unavailable");
        }
        let device_name = context
            .name()
            .map_err(|error| anyhow::anyhow!("could not query CUDA device: {error:?}"))?;
        let (major, minor) = context
            .compute_capability()
            .map_err(|error| anyhow::anyhow!("could not query CUDA capability: {error:?}"))?;
        let options = CompileOptions {
            options: vec![format!("--gpu-architecture=compute_{major}{minor}")],
            ..Default::default()
        };
        let ptx =
            compile_ptx_with_opts(include_str!("gpu_repulsion.cu"), options).map_err(|error| {
                anyhow::anyhow!("could not compile CUDA repulsion kernel: {error:?}")
            })?;
        let module = context
            .load_module(ptx)
            .map_err(|error| anyhow::anyhow!("could not load CUDA repulsion kernel: {error:?}"))?;
        let function = module
            .load_function("repulsion")
            .map_err(|error| anyhow::anyhow!("could not find CUDA repulsion kernel: {error:?}"))?;
        let stream = context.default_stream();
        log::info!("GPU layout selected {device_name} via CUDA");
        Ok(Self {
            _context: context,
            stream,
            function,
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
        if positions.is_empty()
            || output.len() != positions.len()
            || positions.len() > u32::MAX as usize
        {
            return Err("CUDA layout buffer lengths are invalid");
        }
        let gpu_nodes: Vec<CudaNode> = nodes
            .iter()
            .map(|node| CudaNode {
                bounds: [node.center[0], node.center[1], node.half, node.charge],
                mass: [node.center_of_charge[0], node.center_of_charge[1], 0.0, 0.0],
                range: [node.first, node.end, 0, 0],
                children: node.children,
            })
            .collect();
        let order: Vec<u32> = ordered.iter().map(|point| point.index).collect();
        let device_nodes = self
            .stream
            .clone_htod(&gpu_nodes)
            .map_err(|_| "CUDA tree upload failed")?;
        let device_order = self
            .stream
            .clone_htod(&order)
            .map_err(|_| "CUDA particle order upload failed")?;
        let device_positions = self
            .stream
            .clone_htod(positions)
            .map_err(|_| "CUDA positions upload failed")?;
        let mut device_forces = self
            .stream
            .alloc_zeros::<Pos2>(positions.len())
            .map_err(|_| "CUDA output allocation failed")?;
        let count = positions.len() as u32;
        let config = LaunchConfig {
            grid_dim: (count.div_ceil(128), 1, 1),
            block_dim: (128, 1, 1),
            shared_mem_bytes: 0,
        };
        unsafe {
            self.stream
                .launch_builder(&self.function)
                .arg(&device_nodes)
                .arg(&device_order)
                .arg(&device_positions)
                .arg(&mut device_forces)
                .arg(&count)
                .arg(&theta)
                .launch(config)
        }
        .map_err(|_| "CUDA repulsion launch failed")?;
        self.stream
            .memcpy_dtoh(&device_forces, output)
            .map_err(|_| "CUDA repulsion readback failed")?;
        self.stream
            .synchronize()
            .map_err(|_| "CUDA repulsion synchronization failed")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuda_kernel_compiles_for_hopper() {
        if !unsafe { cudarc::nvrtc::sys::is_culib_present() } {
            eprintln!("skipping CUDA kernel compilation: NVRTC unavailable");
            return;
        }
        let options = CompileOptions {
            options: vec!["--gpu-architecture=compute_90".into()],
            ..Default::default()
        };
        compile_ptx_with_opts(include_str!("gpu_repulsion.cu"), options).unwrap();
    }
}
