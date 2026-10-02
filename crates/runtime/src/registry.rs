//! wasi-nn graph (ML model) loading and caching.
//!
//! Graphs are big — a production model weighs ~2.3 GiB resident — and slow to
//! load (seconds: the OpenVINO plugin reads and compiles the network). So they
//! are loaded once per process in [`CachedGraphRegistry`], shared by every
//! `StoreBuilder` of a `WasmEngine`, and an executor resolves its app's graphs
//! when it is created (`StoreBuilder::preload_wasi_nn`, on the blocking pool),
//! not on the request path. Each `Store` then only clones `Graph` handles
//! (`Arc`s) into its `WasiNnCtx`.
//!
//! Which graphs an app gets is driven by its env: `FASTEDGE_NN_BACKENDS` lists
//! the backends (`openvino`, `candle`), and every key starting with
//! `FASTEDGE_NN_<BACKEND>` names a directory the backend can load a graph from.
//! The guest refers to the graph by that directory's last path component.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use moka::sync::Cache;
use wasmtime_wasi_nn::backend::candle::CandleBackend;
use wasmtime_wasi_nn::wit::types::GraphEncoding;
use wasmtime_wasi_nn::{
    GraphRegistry, Registry,
    backend::openvino::OpenvinoBackend,
    wit::types::ExecutionTarget,
    {Backend, Graph},
};

/// `(backend kind, graph directory)` pairs an app asks to have loaded.
pub type PreloadSpec = Vec<(String, String)>;

/// Process-wide cache of loaded graphs, keyed by graph name.
///
/// Cheap to clone (the cache is reference-counted); one instance lives on the
/// `WasmEngine` and is handed to every `StoreBuilder` it creates.
#[derive(Clone)]
pub struct CachedGraphRegistry(Cache<String, Graph>);

impl Default for CachedGraphRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The graphs one `Store` can see, by name.
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

/// Reads an app's graph preload requests from its env.
///
/// `FASTEDGE_NN_BACKENDS=openvino,candle` selects the backends; for each, every
/// env key starting with `FASTEDGE_NN_<BACKEND>` (backend upper-cased) holds a
/// graph directory. Order follows the backend list, then the env order.
pub fn preload_spec<K: AsRef<str>, V: AsRef<str>>(env: &[(K, V)]) -> PreloadSpec {
    let Some((_, backends)) = env
        .iter()
        .find(|(k, _)| k.as_ref() == "FASTEDGE_NN_BACKENDS")
    else {
        return Vec::new();
    };

    backends
        .as_ref()
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .flat_map(|backend| {
            let prefix = format!("FASTEDGE_NN_{}", backend.to_ascii_uppercase());
            env.iter().filter_map(move |(k, v)| {
                k.as_ref()
                    .starts_with(&prefix)
                    .then(|| (backend.to_string(), v.as_ref().to_string()))
            })
        })
        .collect()
}

/// The name a graph is registered under: its directory's last path component.
fn graph_name(path: &str) -> Result<String> {
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("no file name in graph path: {}", path))
}

/// Loads one graph from `path` with the backend `kind`. Slow and memory-hungry;
/// only ever called through the cache.
fn load_graph(kind: &str, path: &str) -> Result<Graph> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        bail!("preload directory is not a valid directory: {}", path);
    }
    let encoding = kind.parse().unwrap_or(GraphEncoding::Autodetect);
    let mut backends = backends();
    let backend = backends
        .iter_mut()
        .find(|b| b.encoding() == encoding)
        .ok_or_else(|| anyhow!("unsupported backend: {}", kind))?
        .as_dir_loadable()
        .ok_or_else(|| anyhow!("{} does not support directory loading", kind))?;
    Ok(backend.load_from_dir(dir, ExecutionTarget::Cpu)?)
}

impl CachedGraphRegistry {
    pub fn new() -> Self {
        let builder = Cache::builder()
            // Max 100 entries
            .max_capacity(100)
            // Time to idle (TTI):  30 minutes
            .time_to_idle(Duration::from_secs(30 * 60));

        Self(builder.build())
    }

    /// Resolves every graph in `spec`, loading the ones not cached yet.
    ///
    /// Concurrent requests for the same graph wait for a single load instead of
    /// each running one (`try_get_with`): a cold graph used to be loaded once
    /// per concurrent miss, each copy costing the full model footprint.
    pub fn load_graphs(&self, spec: &[(String, String)]) -> Result<Vec<(String, Graph)>> {
        spec.iter()
            .map(|(kind, path)| {
                let name = graph_name(path)?;
                let graph = self
                    .0
                    .try_get_with(name.clone(), || load_graph(kind, path))
                    .map_err(|error| anyhow!("wasi-nn backend load {}: {:#}", path, error))?;
                Ok((name, graph))
            })
            .collect()
    }

    /// Builds a `Store`'s view of already-resolved graphs. Only `Arc` clones.
    pub fn registry_for(graphs: &[(String, Graph)]) -> Registry {
        Registry::from(StoreRegistry(graphs.iter().cloned().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn preload_spec_is_empty_without_backends() {
        let e = env(&[("FASTEDGE_NN_OPENVINO_MODEL", "/models/a"), ("X", "y")]);
        assert!(preload_spec(&e).is_empty());
    }

    #[test]
    fn preload_spec_collects_dirs_per_backend_in_order() {
        let e = env(&[
            ("FASTEDGE_NN_OPENVINO_MODEL_B", "/models/b"),
            ("FASTEDGE_NN_BACKENDS", "openvino, candle"),
            ("FASTEDGE_NN_CANDLE_MODEL", "/models/c"),
            ("FASTEDGE_NN_OPENVINO_MODEL_A", "/models/a"),
            ("UNRELATED", "/models/z"),
        ]);
        assert_eq!(
            preload_spec(&e),
            vec![
                ("openvino".to_string(), "/models/b".to_string()),
                ("openvino".to_string(), "/models/a".to_string()),
                ("candle".to_string(), "/models/c".to_string()),
            ]
        );
    }

    #[test]
    fn preload_spec_matches_backend_prefix_case_insensitively() {
        let e = env(&[
            ("FASTEDGE_NN_BACKENDS", "OpenVINO"),
            ("FASTEDGE_NN_OPENVINO", "/models/a"),
        ]);
        assert_eq!(
            preload_spec(&e),
            vec![("OpenVINO".to_string(), "/models/a".to_string())]
        );
    }

    #[test]
    fn load_graphs_rejects_a_missing_directory_naming_it() {
        let registry = CachedGraphRegistry::new();
        let spec = vec![("openvino".to_string(), "/definitely/not/here".to_string())];
        let error = registry
            .load_graphs(&spec)
            .err()
            .expect("missing dir must fail")
            .to_string();
        assert!(error.contains("/definitely/not/here"), "{error}");
    }

    #[test]
    fn load_graphs_rejects_an_unsupported_backend() {
        // A valid encoding no configured backend provides. (An unparseable kind
        // falls back to `Autodetect`, which a backend does accept.)
        let dir = std::env::temp_dir();
        let registry = CachedGraphRegistry::new();
        let spec = vec![("onnx".to_string(), dir.to_string_lossy().into_owned())];
        let error = registry
            .load_graphs(&spec)
            .err()
            .expect("unknown backend must fail")
            .to_string();
        assert!(error.contains("unsupported backend"), "{error}");
    }

    #[test]
    fn registry_for_exposes_graphs_by_name_and_empty_is_fine() {
        let registry = CachedGraphRegistry::registry_for(&[]);
        assert!(registry.get("anything").is_none());
    }
}
