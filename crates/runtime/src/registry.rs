//! wasi-nn graph (ML model) loading and caching.
//!
//! Graphs are big — a production model weighs ~2.3 GiB resident — and slow to
//! load (seconds: the OpenVINO plugin reads and compiles the network). So they
//! are loaded once per process in [`CachedGraphRegistry`], owned by the
//! embedder's context (`ContextT::graph_registry`) and shared by every
//! `WasmEngine` built from it and every `StoreBuilder` of those, and an executor resolves its app's graphs
//! when it is created (`StoreBuilder::preload_wasi_nn`, on the blocking pool),
//! not on the request path. Each `Store` then only clones `Graph` handles
//! (`Arc`s) into its `WasiNnCtx`.
//!
//! Which graphs an app gets is driven by its env: `FASTEDGE_NN_BACKENDS` lists
//! the backends (`openvino`, `candle`), and every key starting with
//! `FASTEDGE_NN_<BACKEND>` names a directory the backend can load a graph from.
//! The guest refers to the graph by that directory's last path component.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use moka::sync::Cache;
use wasmtime_wasi_nn::backend::candle::CandleBackend;
use wasmtime_wasi_nn::wit::WasiNnCtx;
use wasmtime_wasi_nn::wit::types::GraphEncoding;
use wasmtime_wasi_nn::{
    GraphRegistry, Registry,
    backend::openvino::OpenvinoBackend,
    wit::types::ExecutionTarget,
    {Backend, Graph},
};

/// How long a graph stays loaded with no `Store` using it.
///
/// Long on purpose: dropping a graph only pays off for a model nobody asks for
/// any more (a replaced one), while expiring one that is merely quiet costs a
/// multi-second reload and, briefly, a second resident copy next to the handles
/// idle executors still hold. Hours, not minutes.
pub const GRAPH_TIME_TO_IDLE: Duration = Duration::from_secs(4 * 60 * 60);

/// `(backend kind, graph directory)` pairs an app asks to have loaded.
pub type PreloadSpec = Vec<(String, String)>;

/// What the process-wide cache is keyed by: the backend and the graph's
/// canonical directory. Two apps pointing at `/tenant-a/model` and
/// `/tenant-b/model` are two graphs; the name the guest sees (`model`) is a
/// per-store matter ([`CachedGraphRegistry::registry_for`]), never the key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GraphKey {
    kind: String,
    path: PathBuf,
}

impl GraphKey {
    fn new(kind: &str, path: &str) -> Result<Self> {
        // Canonical, so `/models/current -> /models/v2` and `/models/v2` are one
        // graph, and a re-pointed symlink is a new one.
        let path = std::fs::canonicalize(path)
            .map_err(|_| anyhow!("preload directory is not a valid directory: {}", path))?;
        Ok(Self {
            kind: kind.to_ascii_lowercase(),
            path,
        })
    }
}

/// One graph as resolved for a store builder: the shared cache entry it came
/// from, the name the guest loads it by, and the handle.
#[derive(Clone)]
pub struct LoadedGraph {
    pub key: GraphKey,
    pub name: String,
    pub graph: Graph,
}

/// Process-wide cache of loaded graphs, keyed by [`GraphKey`].
///
/// Cheap to clone (the cache is reference-counted); one instance lives on the
/// embedder's context, is shared by every `WasmEngine` built from it, and is
/// handed to every `StoreBuilder` those engines create.
///
/// Entries are strong: an executor rebuild (config change, debug toggle) finds
/// the graph here and never reloads it. A graph is dropped after
/// [`GRAPH_TIME_TO_IDLE`] without *traffic* — executors only read the cache at
/// preload, so every `Store` creation refreshes its graphs' idle clocks through
/// [`CachedGraphRegistry::mark_used`]. Without that, a model under constant
/// load would expire on schedule and the next executor rebuild would load a
/// second ~2.3 GiB copy next to the one live executors still hold.
#[derive(Clone)]
pub struct CachedGraphRegistry(Cache<GraphKey, Graph>);

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
        Self::with_time_to_idle(GRAPH_TIME_TO_IDLE)
    }

    fn with_time_to_idle(time_to_idle: Duration) -> Self {
        let builder = Cache::builder()
            // Max 100 entries
            .max_capacity(100)
            .time_to_idle(time_to_idle);

        Self(builder.build())
    }

    /// Records that `graphs` are in use, so their entries do not expire while
    /// requests keep coming. Called on every `Store` creation; a Moka read is
    /// a hash lookup plus a buffered access note, nothing on the order of the
    /// store itself.
    pub fn mark_used(&self, graphs: &[LoadedGraph]) {
        for g in graphs {
            let _ = self.0.get(&g.key);
        }
    }

    /// Resolves every graph in `spec`, loading the ones not cached yet.
    ///
    /// Concurrent requests for the same graph wait for a single load instead of
    /// each running one (`try_get_with`): a cold graph used to be loaded once
    /// per concurrent miss, each copy costing the full model footprint. The
    /// handles returned keep the graph alive for the executor's lifetime; the
    /// cache entry's own lifetime is a matter of traffic (see [`Self::mark_used`]).
    pub fn load_graphs(&self, spec: &[(String, String)]) -> Result<Vec<LoadedGraph>> {
        let mut graphs: Vec<LoadedGraph> = Vec::with_capacity(spec.len());
        for (kind, path) in spec {
            let key = GraphKey::new(kind, path)?;
            let name = graph_name(path)?;
            // The guest addresses graphs by name, so within one app two
            // directories with the same basename would shadow each other.
            if let Some(dup) = graphs.iter().find(|g| g.name == name) {
                bail!(
                    "wasi-nn graph name {:?} used by both {} and {}",
                    name,
                    dup.key.path.display(),
                    path
                );
            }
            let graph = self
                .0
                .try_get_with(key.clone(), || load_graph(kind, path))
                .map_err(|error| anyhow!("wasi-nn backend load {}: {:#}", path, error))?;
            graphs.push(LoadedGraph { key, name, graph });
        }
        Ok(graphs)
    }

    /// Builds a `Store`'s view of already-resolved graphs. Only `Arc` clones.
    pub fn registry_for(graphs: &[LoadedGraph]) -> Registry {
        Registry::from(StoreRegistry(
            graphs
                .iter()
                .map(|g| (g.name.clone(), g.graph.clone()))
                .collect(),
        ))
    }

    /// A `Store`'s wasi-nn context over already-resolved graphs: the backends
    /// plus [`Self::registry_for`]. Only `Arc` clones and a few small allocs.
    pub fn ctx_for(graphs: &[LoadedGraph]) -> WasiNnCtx {
        WasiNnCtx::new(backends(), Self::registry_for(graphs))
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

    /// A graph that cannot run; enough to occupy a cache slot.
    struct StubGraph;

    impl wasmtime_wasi_nn::backend::BackendGraph for StubGraph {
        fn init_execution_context(
            &self,
        ) -> std::result::Result<
            wasmtime_wasi_nn::ExecutionContext,
            wasmtime_wasi_nn::backend::BackendError,
        > {
            Err(wasmtime_wasi_nn::backend::BackendError::NoInputs)
        }
    }

    fn stub_graph() -> Graph {
        Graph::from(Box::new(StubGraph) as Box<dyn wasmtime_wasi_nn::backend::BackendGraph>)
    }

    /// The idle clock must follow store creations, not preloads: a model that
    /// serves requests continuously stays cached past its idle timeout, and
    /// one that stops being used is dropped.
    #[test]
    fn mark_used_keeps_a_busy_graph_cached_until_traffic_stops() {
        let tti = Duration::from_millis(300);
        let registry = CachedGraphRegistry::with_time_to_idle(tti);
        let key = GraphKey {
            kind: "stub".into(),
            path: "/models/m".into(),
        };
        registry.0.insert(key.clone(), stub_graph());
        let held = vec![LoadedGraph {
            key: key.clone(),
            name: "m".into(),
            graph: registry.0.get(&key).unwrap(),
        }];

        // Busy for 3x the idle timeout, touched every third of it.
        for _ in 0..9 {
            std::thread::sleep(tti / 3);
            registry.mark_used(&held);
        }
        assert!(registry.0.get(&key).is_some(), "busy graph expired");

        // Idle for 2x the timeout; still held by `held`, like an idle executor
        // would, which must not keep the cache entry alive.
        std::thread::sleep(tti * 2);
        assert!(registry.0.get(&key).is_none(), "idle graph still cached");
    }

    /// Same basename, different directories: two cache entries, each app gets
    /// its own graph under the name `model` — and a different backend for the
    /// same directory is a third.
    #[test]
    fn graph_key_separates_same_basename_under_different_paths_and_backends() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("tenant-a").join("model");
        let b = root.path().join("tenant-b").join("model");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());

        let ka = GraphKey::new("openvino", a).unwrap();
        let kb = GraphKey::new("openvino", b).unwrap();
        let kc = GraphKey::new("candle", a).unwrap();
        assert_ne!(ka, kb);
        assert_ne!(ka, kc);
        assert_eq!(
            ka,
            GraphKey::new("OpenVINO", a).unwrap(),
            "kind is case-insensitive"
        );
        assert_eq!(graph_name(a).unwrap(), "model");
        assert_eq!(graph_name(b).unwrap(), "model");
    }

    /// Within one app two directories with the same basename cannot both be
    /// addressed by the guest, so that is a config error, not a silent shadow.
    #[test]
    fn load_graphs_rejects_duplicate_names_within_an_app() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a").join("model");
        let b = root.path().join("b").join("model");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let registry = CachedGraphRegistry::with_time_to_idle(Duration::from_secs(60));
        registry.0.insert(
            GraphKey::new("openvino", a.to_str().unwrap()).unwrap(),
            stub_graph(),
        );
        let error = registry
            .load_graphs(&[
                ("openvino".into(), a.to_str().unwrap().into()),
                ("openvino".into(), b.to_str().unwrap().into()),
            ])
            .err()
            .expect("duplicate name must fail")
            .to_string();
        assert!(error.contains("\"model\""), "{error}");
    }

    #[test]
    fn registry_for_exposes_graphs_by_name_and_empty_is_fine() {
        let registry = CachedGraphRegistry::registry_for(&[]);
        assert!(registry.get("anything").is_none());
    }
}
