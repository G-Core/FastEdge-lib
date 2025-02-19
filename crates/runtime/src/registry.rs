use std::collections::HashMap;
use std::ops::Deref;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use moka::sync::Cache;
use wasmtime_wasi_nn::backend::candle::CandleBackend;
use wasmtime_wasi_nn::wit::types::GraphEncoding;
use wasmtime_wasi_nn::{
    backend::{openvino::OpenvinoBackend, BackendFromDir},
    wit::types::ExecutionTarget,
    GraphRegistry, Registry, {Backend, Graph},
};

#[derive(Clone)]
pub struct CachedGraphRegistry(Cache<String, Graph>);

pub struct StoreRegistry(HashMap<String, Graph>);

pub fn backends() -> Vec<Backend> {
    vec![
        Backend::from(OpenvinoBackend::default()),
        Backend::from(CandleBackend::default()),
    ]
}

impl GraphRegistry for StoreRegistry {
    fn get(&self, name: &str) -> Option<&Graph> {
        self.0.get(name)
    }

    fn get_mut(&mut self, name: &str) -> Option<&mut Graph> {
        self.0.get_mut(name)
    }
}

impl CachedGraphRegistry {
    pub fn new() -> Self {
        let builder = Cache::builder()
            // Max 10,000 entries
            .max_capacity(100)
            // Time to idle (TTI):  30 minutes
            .time_to_idle(Duration::from_secs(30 * 60));

        Self(builder.build())
    }

    /// Load a graph from the files contained in the `path` directory.
    ///
    /// This expects the backend to know how to load graphs (i.e., ML model)
    /// from a directory. The name used in the registry is the directory's last
    /// suffix: if the backend can find the files it expects in `/my/model/foo`,
    /// the registry will contain a new graph named `foo`.
    fn load(&self, backend: &mut dyn BackendFromDir, path: impl AsRef<Path>) -> Result<Graph> {
        let path = path.as_ref();
        if !path.is_dir() {
            bail!(
                "preload directory is not a valid directory: {}",
                path.display()
            );
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy())
            .ok_or(anyhow!("no file name in path"))?;

        let graph = backend.load_from_dir(path, ExecutionTarget::Cpu)?;
        self.0.insert(name.into_owned(), graph.clone());
        Ok(graph)
    }

    pub fn preload_graphs(
        &self,
        preload: Vec<(String, String)>,
    ) -> Result<(impl IntoIterator<Item = Backend>, Registry)> {
        let mut backends = backends();
        let mut registry = StoreRegistry(HashMap::new());
        for (kind, path) in preload {
            let name = Path::new(&path)
                .file_name()
                .map(|s| s.to_string_lossy())
                .ok_or(anyhow!("no file name in path"))?;
            let graph = if let Some(graph) = self.0.get(name.deref()) {
                graph
            } else {
                let kind_ = kind.parse().unwrap_or(GraphEncoding::Autodetect);
                let backend = backends
                    .iter_mut()
                    .find(|b| b.encoding() == kind_)
                    .ok_or(anyhow!("unsupported backend: {}", kind))?
                    .as_dir_loadable()
                    .ok_or(anyhow!("{} does not support directory loading", kind))?;
                self.load(backend, &path)
                    .with_context(|| format!("wasi-nn backend load {}", path))?
            };
            registry.0.insert(name.into_owned(), graph);
        }
        Ok((backends, Registry::from(registry)))
    }
}
