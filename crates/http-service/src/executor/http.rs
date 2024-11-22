use crate::executor::HttpExecutor;
use crate::state::HttpState;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use bytesize::ByteSize;
use dictionary::Dictionary;
use http::{Method, Request, Response, StatusCode};
use http_backend::Backend;
use http_body_util::{BodyExt, Full};
use hyper::body::Body;
use reactor::gcore::fastedge;
use runtime::{store::StoreBuilder, InstancePre};
use secret::{Secret, SecretStrategy};
use std::time::{Duration, Instant};
use wasmtime_wasi::StdoutStream;
use wasmtime_wasi_http::body::HyperOutgoingBody;

/// Execute context used by ['HttpService']
#[derive(Clone)]
pub struct HttpExecutorImpl<C, T: SecretStrategy> {
    instance_pre: InstancePre<HttpState<C, T>>,
    store_builder: StoreBuilder,
    backend: Backend<C>,
    dictionary: Dictionary,
    secret: Secret<T>,
}

#[async_trait]
impl<C, T> HttpExecutor for HttpExecutorImpl<C, T>
where
    C: Clone + Send + Sync + 'static,
    T: SecretStrategy + Clone + Send + Sync,
{
    async fn execute<B, R>(
        &self,
        req: Request<B>,
        on_response: R,
    ) -> anyhow::Result<Response<HyperOutgoingBody>>
    where
        R: FnOnce(StatusCode, ByteSize, Duration) + Send + 'static,
        B: BodyExt + Send,
        <B as Body>::Data: Send,
    {
        tracing::trace!("start execute");
        let start_ = Instant::now();

        let (parts, body) = req.into_parts();
        let method = to_fastedge_http_method(&parts.method)?;

        let headers = parts
            .headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .map(|v| (name.to_string(), v.to_string()))
                    .ok()
            })
            .collect::<Vec<(String, String)>>();

        let body = body
            .collect()
            .await
            .map_err(|_| anyhow!("body read error"))?
            .to_bytes();
        let body = if body.is_empty() {
            None
        } else {
            Some(body.to_vec())
        };

        let request = fastedge::http::Request {
            method,
            uri: parts.uri.to_string(),
            headers,
            body,
        };

        let properties = crate::executor::get_properties(&parts.headers);

        let store_builder = self.store_builder.to_owned().with_properties(properties);
        let wasi_nn = self.store_builder.make_wasi_nn_ctx()?;
        let mut http_backend = self.backend.to_owned();

        http_backend
            .propagate_headers(parts.headers.clone())
            .context("propagate headers")?;

        let propagate_header_names = http_backend.propagate_header_names();
        let backend_uri = http_backend.uri();
        let state = HttpState {
            wasi_nn,
            http_backend,
            uri: backend_uri,
            propagate_headers: parts.headers,
            propagate_header_names,
            dictionary: self.dictionary.clone(),
            secret: self.secret.clone(),
        };

        let mut store = store_builder.build(state)?;

        let instance = self.instance_pre.instantiate_async(&mut store).await?;
        let func = instance
            .exports(&mut store)
            .instance("gcore:fastedge/http-handler")
            .ok_or_else(|| anyhow!("gcore:fastedge/http-handler instance not found"))?
            .typed_func::<(fastedge::http::Request,), (fastedge::http::Response,)>("process")?;
        let duration = Duration::from_millis(store.data().timeout);
        let func = tokio::time::timeout(duration, func.call_async(&mut store, (request,)));
        let (resp,) = match func.await? {
            Ok(res) => res,
            Err(error) => {
                // log to application logger  error
                if let Some(ref logger) = store.data().logger {
                    if let Err(e) = logger.stream().write(error.to_string().into()) {
                        tracing::debug!(cause=?e, "write error: {}", error)
                    }
                }
                return Err(error);
            }
        };
        let status_code = ::http::StatusCode::try_from(resp.status)?;
        let builder = ::http::Response::builder().status(status_code);
        let builder = if let Some(headers) = resp.headers {
            headers
                .iter()
                .fold(builder, |builder, (k, v)| builder.header(k, v))
        } else {
            builder
        };

        let elapsed = Instant::now().duration_since(start_);
        on_response(
            status_code,
            ByteSize::b(store.memory_used() as u64),
            elapsed,
        );

        let body = resp
            .body
            .map(|b| Full::from(b).map_err(|never| match never {}).boxed())
            .unwrap_or_default();
        builder.body(body).map_err(anyhow::Error::msg)
    }
}

impl<C, T> HttpExecutorImpl<C, T>
where
    C: Clone + Send + Sync + 'static,
    T: SecretStrategy + Clone + Send,
{
    pub fn new(
        instance_pre: InstancePre<HttpState<C, T>>,
        store_builder: StoreBuilder,
        backend: Backend<C>,
        dictionary: Dictionary,
        secret: Secret<T>,
    ) -> Self {
        Self {
            instance_pre,
            store_builder,
            backend,
            dictionary,
            secret,
        }
    }
}

fn to_fastedge_http_method(method: &Method) -> anyhow::Result<fastedge::http::Method> {
    Ok(match method {
        &Method::GET => fastedge::http::Method::Get,
        &Method::POST => fastedge::http::Method::Post,
        &Method::PUT => fastedge::http::Method::Put,
        &Method::DELETE => fastedge::http::Method::Delete,
        &Method::HEAD => fastedge::http::Method::Head,
        &Method::PATCH => fastedge::http::Method::Patch,
        &Method::OPTIONS => fastedge::http::Method::Options,
        method => bail!("unsupported method: {}", method),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::http::HttpExecutorImpl;
    use crate::{
        ContextHeaders, ExecutorFactory, HttpService, FASTEDGE_EXECUTION_TIMEOUT,
        FASTEDGE_OUT_OF_MEMORY,
    };
    use bytes::Bytes;
    use claims::*;
    use dictionary::Dictionary;
    use http_backend::{Backend, BackendStrategy, FastEdgeConnector};
    use http_body_util::Empty;
    use runtime::app::Status;
    use runtime::logger::{Logger, NullAppender};
    use runtime::service::ServiceBuilder;
    use runtime::util::stats::{StatRow, StatsWriter};
    use runtime::{
        componentize_if_necessary, App, ContextT, PreCompiledLoader, Router, WasiVersion,
        WasmConfig, WasmEngine,
    };
    use secret::Secret;
    use smol_str::{SmolStr, ToSmolStr};
    use std::collections::HashMap;
    use wasmtime::component::Component;
    use wasmtime::{Engine, Module};

    #[derive(Clone)]
    struct TestContext {
        geo: HashMap<SmolStr, SmolStr>,
        app: Option<App>,
        engine: Engine,
    }

    impl ContextT for TestContext {
        type BackendConnector = FastEdgeConnector;

        fn make_logger(&self, _app_name: SmolStr, _wrk: &App) -> Logger {
            Logger::new(NullAppender)
        }

        fn backend(&self) -> Backend<FastEdgeConnector> {
            let backend = Backend::<FastEdgeConnector>::builder(BackendStrategy::FastEdge).build(
                FastEdgeConnector::new(assert_ok!("http://localhost:8080/".parse())),
            );
            backend
        }

        fn loader(&self) -> &dyn PreCompiledLoader<u64> {
            self
        }

        fn engine_ref(&self) -> &Engine {
            &self.engine
        }
    }

    static DUMMY_SAMPLE: &[u8] = include_bytes!("../fixtures/dummy.wasm");
    static ALLOC_SAMPLE: &[u8] = include_bytes!("../fixtures/alloc.wasm");

    impl PreCompiledLoader<u64> for TestContext {
        fn load_component(&self, id: u64) -> anyhow::Result<Component> {
            let bytes = if id == 100 {
                ALLOC_SAMPLE
            } else {
                DUMMY_SAMPLE
            };
            let wasm_sample = componentize_if_necessary(bytes)?;
            Component::new(&self.engine, wasm_sample)
        }

        fn load_module(&self, _id: u64) -> anyhow::Result<Module> {
            Module::new(&self.engine, DUMMY_SAMPLE)
        }
    }

    impl StatsWriter for TestContext {
        fn write_stats(&self, _stat: StatRow) {}
    }

    impl Router for TestContext {
        async fn lookup_by_name(&self, _name: &str) -> Option<App> {
            self.app.clone()
        }

        async fn lookup_by_id(&self, _id: u64) -> Option<(SmolStr, App)> {
            todo!()
        }
    }

    impl ContextHeaders for TestContext {
        fn append_headers(&self) -> impl Iterator<Item = (SmolStr, SmolStr)> {
            self.geo.iter().map(|(k, v)| (k.to_owned(), v.to_owned()))
        }
    }

    #[derive(Clone)]
    struct TestSecret;

    impl SecretStrategy for TestSecret {
        fn get(&self, _key: String) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(Some(b"secret".to_vec()))
        }
    }

    impl ExecutorFactory<HttpState<FastEdgeConnector, TestSecret>> for TestContext {
        type Executor = HttpExecutorImpl<FastEdgeConnector, TestSecret>;

        fn get_executor(
            &self,
            name: SmolStr,
            cfg: &App,
            engine: &WasmEngine<HttpState<FastEdgeConnector, TestSecret>>,
        ) -> anyhow::Result<Self::Executor> {
            let mut dictionary = Dictionary::new();
            for (k, v) in cfg.env.iter() {
                dictionary.insert(k.to_string(), v.to_string());
            }
            let secret = Secret::new(TestSecret);

            let env = cfg.env.iter().collect::<Vec<(&SmolStr, &SmolStr)>>();

            let logger = self.make_logger(name.clone(), cfg);

            let version = WasiVersion::Preview2;
            let store_builder = engine
                .store_builder(version)
                .set_env(&env)
                .max_memory_size(cfg.mem_limit)
                .max_epoch_ticks(cfg.max_duration)
                .logger(logger);

            let component = self.loader().load_component(cfg.binary_id)?;
            let instance_pre = engine.component_instantiate_pre(&component)?;
            tracing::info!("Added '{}' to cache", name);
            Ok(HttpExecutorImpl::new(
                instance_pre,
                store_builder,
                self.backend(),
                dictionary,
                secret,
            ))
        }
    }

    fn default_test_app(status: Status) -> Option<App> {
        Some(App {
            binary_id: 0,
            max_duration: 10,
            mem_limit: 1400000,
            env: Default::default(),
            rsp_headers: HashMap::from([
                ("RES_HEADER_01".to_smolstr(), "01".to_smolstr()),
                ("RES_HEADER_02".to_smolstr(), "02".to_smolstr()),
            ]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_smolstr(),
            status,
            debug_until: None,
            secrets: vec![],
        })
    }

    fn make_engine() -> Engine {
        let config = WasmConfig::default();
        let engine = assert_ok!(Engine::new(&config));
        engine
    }

    fn load_geo_info() -> HashMap<SmolStr, SmolStr> {
        let mut ret = HashMap::new();
        ret.insert("PoP-Lat".to_smolstr(), "47.00420".to_smolstr());
        ret.insert("PoP-Long".to_smolstr(), "28.85740".to_smolstr());
        ret.insert("PoP-Reg".to_smolstr(), "CU".to_smolstr());
        ret.insert("PoP-City".to_smolstr(), "Bucharest".to_smolstr());
        ret.insert("PoP-Continent".to_smolstr(), "EU".to_smolstr());
        ret.insert("PoP-Country-Code".to_smolstr(), "RO".to_smolstr());
        ret.insert("PoP-Country-Name".to_smolstr(), "Romania".to_smolstr());
        ret
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_success() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "success.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Enabled),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext, TestSecret> =
            assert_ok!(ServiceBuilder::new(context).build());

        let res = assert_ok!(http_service.handle_request("1", req).await);
        assert_eq!(StatusCode::OK, res.status());
        let headers = res.headers();
        assert_eq!(4, headers.len());
        assert_eq!(
            "*",
            assert_some!(headers.get("access-control-allow-origin"))
        );
        assert_eq!("no-store", assert_some!(headers.get("cache-control")));
        assert_eq!("01", assert_some!(headers.get("RES_HEADER_01")));
        assert_eq!("02", assert_some!(headers.get("RES_HEADER_02")));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_timeout() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "timeout.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let app = Some(App {
            binary_id: 1,
            max_duration: 0,
            mem_limit: 10000000,
            env: Default::default(),
            rsp_headers: HashMap::from([("RES_HEADER_03".to_smolstr(), "03".to_smolstr())]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_smolstr(),
            status: Status::Enabled,
            debug_until: None,
            secrets: vec![],
        });

        let context = TestContext {
            geo: load_geo_info(),
            app,
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext, TestSecret> =
            assert_ok!(ServiceBuilder::new(context).build());

        let res = assert_ok!(http_service.handle_request("2", req).await);
        assert_eq!(FASTEDGE_EXECUTION_TIMEOUT, res.status());
        let headers = res.headers();
        assert_eq!(3, headers.len());
        assert_eq!(
            "*",
            assert_some!(headers.get("access-control-allow-origin"))
        );
        assert_eq!("no-store", assert_some!(headers.get("cache-control")));
        assert_eq!("03", assert_some!(headers.get("RES_HEADER_03")));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_insufficient_memory() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/?size=200000")
            .header("server_name", "insufficient_memory.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let app = Some(App {
            binary_id: 100,
            max_duration: 10,
            mem_limit: 1500000,
            env: Default::default(),
            rsp_headers: HashMap::from([("RES_HEADER_03".to_smolstr(), "03".to_smolstr())]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_smolstr(),
            status: Status::Enabled,
            debug_until: None,
            secrets: vec![],
        });

        let context = TestContext {
            geo: load_geo_info(),
            app,
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext, TestSecret> =
            assert_ok!(ServiceBuilder::new(context).build());

        let res = assert_ok!(http_service.handle_request("3", req).await);
        assert_eq!(FASTEDGE_OUT_OF_MEMORY, res.status());
        let headers = res.headers();
        assert_eq!(3, headers.len());
        assert_eq!(
            "*",
            assert_some!(headers.get("access-control-allow-origin"))
        );
        assert_eq!("no-store", assert_some!(headers.get("cache-control")));
        assert_eq!("03", assert_some!(headers.get("RES_HEADER_03")));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn draft_app() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "draft.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Draft),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext, TestSecret> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request("4", req).await);
        assert_eq!(StatusCode::NOT_FOUND, res.status());
        assert_eq!(0, res.headers().len());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn disabled_app() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "draft.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Disabled),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext, TestSecret> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request("5", req).await);
        assert_eq!(StatusCode::NOT_FOUND, res.status());
        assert_eq!(0, res.headers().len());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn rate_limit_app() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "draft.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::RateLimited),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext, TestSecret> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request("6", req).await);
        assert_eq!(StatusCode::TOO_MANY_REQUESTS, res.status());
        assert_eq!(0, res.headers().len());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn suspended_app() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "draft.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Suspended),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext, TestSecret> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request("7", req).await);
        assert_eq!(StatusCode::NOT_ACCEPTABLE, res.status());
        assert_eq!(0, res.headers().len());
    }
}
