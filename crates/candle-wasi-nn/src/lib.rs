use std::fs::File;
use std::io;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use candle::Device;
use candle_core as candle;
use candle_core::{DType, IndexOp};
use candle_nn::var_builder::VarBuilder;
use candle_transformers::models::{
    llama2_c::{Cache, Config, Llama},
    llama2_c_weights::TransformerWeights,
};
use tracing::{trace, warn};
use wasmtime_wasi_nn::{
    backend::{BackendError, BackendExecutionContext, BackendFromDir, BackendGraph, BackendInner},
    wit::types::{ExecutionTarget, GraphEncoding, Tensor},
    {ExecutionContext, Graph},
};

fn io_error(error: io::Error) -> BackendError {
    BackendError::BackendAccess(error.into())
}

fn candle_error(error: candle::Error) -> BackendError {
    BackendError::BackendAccess(error.into())
}

enum Model {
    Llama(Llama),
}

impl Model {
    fn forward(&self, xs: &candle::Tensor, pos: usize) -> Result<candle::Tensor, BackendError> {
        match self {
            Self::Llama(l) => Ok(l.forward(xs, pos).map_err(candle_error)?),
        }
    }
}

#[derive(Default)]
pub struct CandleBackend;

unsafe impl Send for CandleBackend {}

unsafe impl Sync for CandleBackend {}

impl BackendInner for CandleBackend {
    fn encoding(&self) -> GraphEncoding {
        GraphEncoding::Autodetect
    }

    fn load(&mut self, builders: &[&[u8]], target: ExecutionTarget) -> Result<Graph, BackendError> {
        let s = Instant::now();
        if builders.len() != 1 {
            return Err(BackendError::InvalidNumberOfBuilders(1, builders.len()));
        }
        let device = device(target).map_err(candle_error)?;
        let mut cursor = Cursor::new(builders[0]);
        let config = Config::from_reader(&mut cursor).map_err(candle_error)?;
        let weights =
            TransformerWeights::from_reader(&mut cursor, &config, &device).map_err(candle_error)?;
        let vb = weights
            .var_builder(&config, &device)
            .map_err(candle_error)?;
        let box_: Box<dyn BackendGraph> = Box::new(CandleGraph { device, config, vb });
        trace!("load graph: {:.0?}", s.elapsed());
        Ok(box_.into())
    }

    fn as_dir_loadable(&mut self) -> Option<&mut dyn BackendFromDir> {
        Some(self)
    }
}

impl BackendFromDir for CandleBackend {
    fn load_from_dir(
        &mut self,
        path: &Path,
        target: ExecutionTarget,
    ) -> Result<Graph, BackendError> {
        trace!(?path, ?target, "load_from_dir: model.bin");
        let s = Instant::now();
        let mut file = File::open(path.join("model.bin")).map_err(io_error)?;
        let mut weights = vec![];
        file.read_to_end(&mut weights).map_err(io_error)?;
        trace!("read file: {:.0?}", s.elapsed());
        self.load(&[&weights], target)
    }
}

struct CandleGraph {
    device: Device,
    config: Config,
    vb: VarBuilder<'static>,
}

unsafe impl Send for CandleGraph {}

unsafe impl Sync for CandleGraph {}

impl BackendGraph for CandleGraph {
    fn init_execution_context(&self) -> Result<ExecutionContext, BackendError> {
        let _s = Instant::now();
        let tensor =
            candle::Tensor::zeros((2, 3), DType::U32, &self.device).map_err(candle_error)?;
        let vb = self.vb.clone();
        let cache = Cache::new(true, &self.config, vb.pp("rot")).map_err(candle_error)?;
        let model =
            Model::Llama(Llama::load(vb, &cache, self.config.clone()).map_err(candle_error)?);
        let model = Arc::new(model);
        let context: Box<dyn BackendExecutionContext> = Box::new(CandleExecutionContext {
            device: self.device.clone(),
            model,
            tensor,
        });
        trace!("init_execution_context: {:.0?}", _s.elapsed());
        Ok(context.into())
    }
}

struct CandleExecutionContext {
    device: Device,
    model: Arc<Model>,
    tensor: candle_core::Tensor,
}

impl BackendExecutionContext for CandleExecutionContext {
    fn set_input(&mut self, index: u32, tensor: &Tensor) -> Result<(), BackendError> {
        trace!(index, ?tensor, "set_input");
        // transmute array of bytes to [u32]
        let tokens = unsafe {
            core::slice::from_raw_parts(
                tensor.data.as_ptr().cast::<u32>(),
                tensor.data.len() / std::mem::size_of::<u32>(),
            )
        };
        let context_size = if index > 0 { 1 } else { tokens.len() };
        let context = &tokens[tokens.len().saturating_sub(context_size)..];
        self.tensor = candle::Tensor::new(context, &self.device)
            .map_err(candle_error)?
            .unsqueeze(0)
            .map_err(candle_error)?;
        trace!("tensor: {:?}", self.tensor);
        Ok(())
    }

    fn compute(&mut self) -> Result<(), BackendError> {
        let _s = Instant::now();
        let index_pos = 0;
        trace!("forward input: {:?}", self.tensor);
        self.tensor = self.model.forward(&self.tensor, index_pos)?;
        trace!("forward output: {:?} in {:.0?}", self.tensor, _s.elapsed());
        Ok(())
    }

    fn get_output(&mut self, index: u32, destination: &mut [u8]) -> Result<u32, BackendError> {
        trace!(index,  ?self.tensor, "get_output");
        let len = self.tensor.dim(index as usize).map_err(candle_error)? - 1;
        let tensor = self.tensor.i((0, len)).map_err(candle_error)?;
        let blob = tensor.to_vec1::<f32>().map_err(candle_error)?;
        if (blob.len() * 4) > destination.len() {
            return Err(BackendError::NotEnoughMemory(blob.len()));
        }
        let data_slice = blob.as_slice();
        let dest = unsafe {
            core::slice::from_raw_parts(
                data_slice.as_ptr().cast::<u8>(),
                std::mem::size_of_val(data_slice),
            )
        };

        // Copy the tensor data into the destination buffer.
        destination[..dest.len()].copy_from_slice(dest);
        Ok(dest.len() as u32)
    }
}

fn device(target: ExecutionTarget) -> candle::Result<Device> {
    match target {
        ExecutionTarget::Cpu => Ok(Device::Cpu),
        ExecutionTarget::Gpu => {
            if cfg!(feature = "cuda") {
                Ok(Device::new_cuda(0)?)
            } else {
                warn!("Running on CPU, to run on GPU, build this example with `--features cuda`");
                Ok(Device::Cpu)
            }
        }
        ExecutionTarget::Tpu => {
            if cfg!(feature = "metal") {
                Ok(Device::new_metal(0)?)
            } else {
                warn!("Running on CPU, to run on TPU, build this example with `--features metal`");
                Ok(Device::Cpu)
            }
        }
    }
}
