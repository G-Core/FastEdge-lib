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
//! `FASTEDGE_NN_<BACKEND>_PROPERTIES` is the exception: `KEY=VALUE,...` device
//! properties the backend sets before compiling that app's graphs (OpenVINO
//! `NUM_STREAMS`, `INFERENCE_NUM_THREADS`, `ENABLE_CPU_PINNING`, ...).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use moka::ops::compute::Op;
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

/// One graph an app asks to have loaded: the backend kind, the directory, and
/// the device properties the backend sets before compiling it (from
/// `FASTEDGE_NN_<BACKEND>_PROPERTIES`; only OpenVINO honours them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphSpec {
    pub kind: String,
    pub path: String,
    pub properties: Vec<(String, String)>,
}

/// The graphs an app asks to have loaded.
pub type PreloadSpec = Vec<GraphSpec>;

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

/// Identity of a graph directory's *contents* at preload time: name, size and
/// mtime of every regular file in it, plus the device properties the graph is
/// compiled with. A model replaced in place (rsync, cp) changes it, so the next
/// executor build loads the new files instead of serving the cached graph
/// until the process restarts; so does a changed `NUM_STREAMS`, which shapes
/// the compiled graph as much as the files do. One `stat` per file; no hashing
/// of multi-GiB weights.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Revision(u64);

/// What one scan of a graph directory tells: its [`Revision`] and the bytes
/// on disk (for the load log; the resident size is larger, see `rss_bytes`).
#[derive(Clone, Copy, Debug)]
struct DirFiles {
    revision: Revision,
    bytes: u64,
}

impl DirFiles {
    fn scan(dir: &Path, properties: &[(String, String)]) -> Result<Self> {
        let mut entries = std::fs::read_dir(dir)
            .and_then(|it| it.collect::<std::io::Result<Vec<_>>>())
            .with_context(|| format!("read graph directory {}", dir.display()))?;
        entries.sort_by_key(|e| e.file_name());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut bytes = 0;
        for entry in entries {
            let meta = entry
                .metadata()
                .with_context(|| format!("stat {}", entry.path().display()))?;
            if meta.is_file() {
                entry.file_name().hash(&mut hasher);
                meta.len().hash(&mut hasher);
                meta.modified().ok().hash(&mut hasher);
                bytes += meta.len();
            }
        }
        for (key, value) in properties {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        Ok(Self {
            revision: Revision(hasher.finish()),
            bytes,
        })
    }
}

/// This process's resident set, for the before/after of a model load. Linux
/// only (`/proc/self/statm`); `None` elsewhere or on any read error.
fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        Some(resident_pages * 4096)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// A cache entry: the graph plus the revision of the files it was loaded from.
#[derive(Clone)]
struct CachedGraph {
    revision: Revision,
    graph: Graph,
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
pub struct CachedGraphRegistry(Cache<GraphKey, CachedGraph>);

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
/// graph directory — except `FASTEDGE_NN_<BACKEND>_PROPERTIES`, which holds
/// comma-separated `KEY=VALUE` device properties applied to each of that
/// backend's graphs before compiling. Order follows the backend list, then the
/// env order. A malformed property entry is an error naming the key.
pub fn preload_spec<K: AsRef<str>, V: AsRef<str>>(env: &[(K, V)]) -> Result<PreloadSpec> {
    let Some((_, backends)) = env
        .iter()
        .find(|(k, _)| k.as_ref() == "FASTEDGE_NN_BACKENDS")
    else {
        return Ok(Vec::new());
    };

    let mut spec = Vec::new();
    for backend in backends
        .as_ref()
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        let prefix = format!("FASTEDGE_NN_{}", backend.to_ascii_uppercase());
        let properties_key = format!("{prefix}_PROPERTIES");
        let properties = match env.iter().find(|(k, _)| k.as_ref() == properties_key) {
            Some((_, v)) => parse_properties(&properties_key, v.as_ref())?,
            None => Vec::new(),
        };
        for (k, v) in env {
            let k = k.as_ref();
            if k.starts_with(&prefix) && k != properties_key {
                spec.push(GraphSpec {
                    kind: backend.to_string(),
                    path: v.as_ref().to_string(),
                    properties: properties.clone(),
                });
            }
        }
    }
    Ok(spec)
}

/// `KEY=VALUE,KEY=VALUE` → pairs. Blanks around entries, keys and values are
/// trimmed; empty entries (trailing commas) are skipped.
fn parse_properties(key: &str, value: &str) -> Result<Vec<(String, String)>> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (k, v) = entry
                .split_once('=')
                .ok_or_else(|| anyhow!("{key}: expected KEY=VALUE, got {entry:?}"))?;
            let (k, v) = (k.trim(), v.trim());
            if k.is_empty() {
                bail!("{key}: empty property name in {entry:?}");
            }
            Ok((k.to_string(), v.to_string()))
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

/// Loads one graph from `spec.path` with the backend `spec.kind`. Slow and
/// memory-hungry; only ever called through the cache.
fn load_graph(spec: &GraphSpec) -> Result<Graph> {
    let dir = Path::new(&spec.path);
    if !dir.is_dir() {
        bail!("preload directory is not a valid directory: {}", spec.path);
    }
    let encoding = spec.kind.parse().unwrap_or(GraphEncoding::Autodetect);
    // A fresh backend per load: the properties live on its `openvino::Core`,
    // so each graph is compiled with its own, not with whatever a previous
    // load set.
    let mut backend = match encoding {
        GraphEncoding::Openvino => Backend::from(OpenvinoBackend::with_properties(
            spec.properties.iter().cloned(),
        )),
        _ => {
            if !spec.properties.is_empty() {
                tracing::warn!(backend = %spec.kind, path = %spec.path,
                    "wasi-nn device properties are only honoured by the openvino backend; ignored");
            }
            backends()
                .into_iter()
                .find(|b| b.encoding() == encoding)
                .ok_or_else(|| anyhow!("unsupported backend: {}", spec.kind))?
        }
    };
    let backend = backend
        .as_dir_loadable()
        .ok_or_else(|| anyhow!("{} does not support directory loading", spec.kind))?;
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
            .time_to_idle(time_to_idle)
            // Models are few and each is gigabytes: say when one leaves the
            // cache and why. Memory is only freed once executors built on it
            // drop their handles.
            .eviction_listener(|key: Arc<GraphKey>, _: CachedGraph, cause| {
                tracing::info!(
                    backend = %key.kind,
                    path = %key.path.display(),
                    ?cause,
                    "wasi-nn graph dropped from the process cache"
                );
            });

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

    /// Resolves every graph in `spec`, loading the ones not cached yet — or
    /// cached from files that have since changed ([`Revision`]).
    ///
    /// The per-key compute is atomic: concurrent requests for the same graph
    /// wait for a single load instead of each running one (a cold graph used
    /// to be loaded once per concurrent miss, each copy costing the full model
    /// footprint), and a stale entry is replaced in place, so the old graph is
    /// freed as soon as the executors built on it are gone. The handles
    /// returned keep a graph alive for the executor's lifetime; the cache
    /// entry's own lifetime is a matter of traffic (see [`Self::mark_used`]).
    pub fn load_graphs(&self, spec: &[GraphSpec]) -> Result<Vec<LoadedGraph>> {
        let mut graphs: Vec<LoadedGraph> = Vec::with_capacity(spec.len());
        for graph_spec in spec {
            let (kind, path) = (&graph_spec.kind, &graph_spec.path);
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
            let files = DirFiles::scan(&key.path, &graph_spec.properties)?;
            let revision = files.revision;
            let graph = self
                .0
                .entry(key.clone())
                .and_try_compute_with(|current| match current {
                    Some(entry) if entry.value().revision == revision => {
                        tracing::debug!(backend = %kind, path = %path, name = %name,
                            "wasi-nn graph served from the process cache");
                        Ok(Op::Nop)
                    }
                    current => {
                        let replacing = current.is_some();
                        let rss_before = rss_bytes();
                        let started = Instant::now();
                        tracing::info!(backend = %kind, path = %path, name = %name,
                            file_bytes = files.bytes, replacing,
                            properties = ?graph_spec.properties, "loading wasi-nn graph");
                        let graph = load_graph(graph_spec)?;
                        tracing::info!(
                            backend = %kind,
                            path = %path,
                            name = %name,
                            file_bytes = files.bytes,
                            rss_delta_bytes = rss_before
                                .zip(rss_bytes())
                                .map(|(before, after)| after.saturating_sub(before)),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            replacing,
                            "wasi-nn graph loaded into the process cache"
                        );
                        Ok(Op::Put(CachedGraph { revision, graph }))
                    }
                })
                .map_err(|error: anyhow::Error| {
                    tracing::error!(backend = %kind, path = %path, name = %name,
                        error = %format!("{error:#}"), "wasi-nn graph load failed");
                    anyhow!("wasi-nn backend load {}: {:#}", path, error)
                })?
                .into_entry()
                .expect("Nop on a present entry or Put: the entry is there")
                .into_value()
                .graph;
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

    fn spec(kind: &str, path: &str) -> GraphSpec {
        GraphSpec {
            kind: kind.to_string(),
            path: path.to_string(),
            properties: Vec::new(),
        }
    }

    #[test]
    fn preload_spec_is_empty_without_backends() {
        let e = env(&[("FASTEDGE_NN_OPENVINO_MODEL", "/models/a"), ("X", "y")]);
        assert!(preload_spec(&e).unwrap().is_empty());
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
            preload_spec(&e).unwrap(),
            vec![
                spec("openvino", "/models/b"),
                spec("openvino", "/models/a"),
                spec("candle", "/models/c"),
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
            preload_spec(&e).unwrap(),
            vec![spec("OpenVINO", "/models/a")]
        );
    }

    /// The properties key shares the directory prefix, so it must be carved
    /// out of the directory scan and land on every graph of its backend only.
    #[test]
    fn preload_spec_attaches_properties_per_backend_and_not_as_a_dir() {
        let e = env(&[
            ("FASTEDGE_NN_BACKENDS", "openvino,candle"),
            ("FASTEDGE_NN_OPENVINO_MODEL", "/models/a"),
            (
                "FASTEDGE_NN_OPENVINO_PROPERTIES",
                " NUM_STREAMS = 24, ENABLE_CPU_PINNING=NO ,",
            ),
            ("FASTEDGE_NN_CANDLE_MODEL", "/models/c"),
        ]);
        let props = vec![
            ("NUM_STREAMS".to_string(), "24".to_string()),
            ("ENABLE_CPU_PINNING".to_string(), "NO".to_string()),
        ];
        assert_eq!(
            preload_spec(&e).unwrap(),
            vec![
                GraphSpec {
                    properties: props,
                    ..spec("openvino", "/models/a")
                },
                spec("candle", "/models/c"),
            ]
        );
    }

    #[test]
    fn preload_spec_rejects_a_malformed_property_naming_the_key() {
        let e = env(&[
            ("FASTEDGE_NN_BACKENDS", "openvino"),
            ("FASTEDGE_NN_OPENVINO_MODEL", "/models/a"),
            ("FASTEDGE_NN_OPENVINO_PROPERTIES", "NUM_STREAMS=24,PINNING"),
        ]);
        let error = preload_spec(&e).err().expect("must fail").to_string();
        assert!(error.contains("FASTEDGE_NN_OPENVINO_PROPERTIES"), "{error}");
        assert!(error.contains("PINNING"), "{error}");
    }

    #[test]
    fn load_graphs_rejects_a_missing_directory_naming_it() {
        let registry = CachedGraphRegistry::new();
        let spec = vec![spec("openvino", "/definitely/not/here")];
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
        let spec = vec![spec("onnx", &dir.to_string_lossy())];
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

    fn cached(graph: Graph) -> CachedGraph {
        CachedGraph {
            revision: Revision(0),
            graph,
        }
    }

    /// Replacing a model's files in place must be seen by the next executor
    /// build; touching nothing must not (or every rebuild would reload).
    #[test]
    fn revision_follows_the_files_contents_not_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("model.bin");
        std::fs::write(&bin, b"v1").unwrap();
        std::fs::write(dir.path().join("model.xml"), b"<net/>").unwrap();
        let first = DirFiles::scan(dir.path(), &[]).unwrap().revision;
        assert_eq!(
            first,
            DirFiles::scan(dir.path(), &[]).unwrap().revision,
            "stable when untouched"
        );

        std::fs::write(&bin, b"v2-longer").unwrap();
        assert_ne!(
            first,
            DirFiles::scan(dir.path(), &[]).unwrap().revision,
            "size change seen"
        );

        std::fs::write(dir.path().join("extra.bin"), b"x").unwrap();
        let with_extra = DirFiles::scan(dir.path(), &[]).unwrap().revision;
        std::fs::remove_file(dir.path().join("extra.bin")).unwrap();
        assert_ne!(
            with_extra,
            DirFiles::scan(dir.path(), &[]).unwrap().revision,
            "added/removed file seen"
        );
    }

    /// The properties shape the compiled graph, so a change must recompile
    /// (a different revision) and the same set must not.
    #[test]
    fn revision_follows_the_device_properties() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("model.bin"), b"v1").unwrap();
        let streams_24 = [("NUM_STREAMS".to_string(), "24".to_string())];
        let streams_8 = [("NUM_STREAMS".to_string(), "8".to_string())];
        let plain = DirFiles::scan(dir.path(), &[]).unwrap().revision;
        let with_24 = DirFiles::scan(dir.path(), &streams_24).unwrap().revision;
        assert_ne!(plain, with_24, "properties seen");
        assert_eq!(
            with_24,
            DirFiles::scan(dir.path(), &streams_24).unwrap().revision,
            "stable for the same properties"
        );
        assert_ne!(
            with_24,
            DirFiles::scan(dir.path(), &streams_8).unwrap().revision,
            "value change seen"
        );
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
        registry.0.insert(key.clone(), cached(stub_graph()));
        let held = vec![LoadedGraph {
            key: key.clone(),
            name: "m".into(),
            graph: registry.0.get(&key).unwrap().graph,
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
        // Cached at the directory's current revision, so `a` hits and the
        // failure is the duplicate name, not a load.
        registry.0.insert(
            GraphKey::new("openvino", a.to_str().unwrap()).unwrap(),
            CachedGraph {
                revision: DirFiles::scan(&a, &[]).unwrap().revision,
                graph: stub_graph(),
            },
        );
        let error = registry
            .load_graphs(&[
                spec("openvino", a.to_str().unwrap()),
                spec("openvino", b.to_str().unwrap()),
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
