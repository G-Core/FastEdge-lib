mod secret;

use crate::secret::SecretImpl;
use ::secret::Secret;
use async_trait::async_trait;
use bytesize::{ByteSize, MB};
use clap::{Args, Parser, Subcommand};
use dictionary::Dictionary;
use http::{Request, Response, StatusCode};
use http_backend::{Backend, BackendStrategy};
use http_body_util::BodyExt;
use http_service::executor::{
    ExecutorFactory, HttpExecutor, HttpExecutorImpl, WasiHttpExecutorImpl,
};
use http_service::state::HttpState;
use http_service::{ContextHeaders, HttpConfig, HttpService, HyperOutgoingBody};
use hyper::body::Body;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use runtime::app::Status;
use runtime::logger::{Console, Logger};
use runtime::service::{Service, ServiceBuilder};
use runtime::util::stats::{StatRow, StatsWriter};
use runtime::{
    componentize_if_necessary, App, ContextT, ExecutorCache, PreCompiledLoader, Router,
    SecretValue, WasiVersion, WasmConfig, WasmEngine,
};
use smol_str::{SmolStr, ToSmolStr};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal;
use wasmtime::component::Component;
use wasmtime::{Engine, Module};

#[derive(Debug, Parser)]
#[command(name = "fastedge-run")]
#[command(about = "FastEdge test tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Execute http handler
    Http(HttpRunArgs),
}

#[derive(Debug, Args)]
struct HttpRunArgs {
    /// Http service listening port
    #[arg(short, long)]
    port: u16,
    /// Wasm file path
    #[arg(short, long)]
    wasm: PathBuf,
    /// Environment variable list
    #[arg(short, long, value_parser = parse_key_value::<SmolStr, SmolStr >)]
    env: Option<Vec<(SmolStr, SmolStr)>>,
    /// Add header from original request
    #[arg(long = "propagate-header", num_args = 0..)]
    propagate_headers: Vec<SmolStr>,
    /// Execution context headers added to request
    #[arg(long, value_parser = parse_key_value::< SmolStr, SmolStr >)]
    headers: Option<Vec<(SmolStr, SmolStr)>>,
    /// Append sample Geo PoP headers
    #[arg(long, default_value = "false")]
    geo: bool,
    /// Limit memory usage
    #[arg(short)]
    mem_limit: Option<usize>,
    /// Limit execution duration
    #[arg(long)]
    max_duration: Option<u64>,
    /// Enable WASI HTTP interface
    #[arg(long)]
    wasi_http: Option<bool>,
    /// Secret variable list
    #[arg(short, long, value_parser = parse_key_value::<SmolStr, SmolStr >)]
    secret: Option<Vec<(SmolStr, SmolStr)>>,
}

/// Test tool execution context
#[derive(Clone)]
struct CliContext {
    headers: HashMap<SmolStr, SmolStr>,
    engine: Engine,
    app: Option<App>,
    backend: Backend<HttpsConnector<HttpConnector>>,
    wasm_bytes: Vec<u8>,
    wasi_http: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();
    #[cfg(target_family = "unix")]
    let shutdown_coordinator = shellflip::ShutdownCoordinator::new();
    let args = Cli::parse();
    let config = WasmConfig::default();
    let engine = Engine::new(&config)?;

    match args.command {
        Commands::Http(run) => {
            let wasm_bytes = tokio::fs::read(&run.wasm).await?;

            let backend_connector = HttpsConnector::new();
            let mut builder =
                Backend::<HttpsConnector<HttpConnector>>::builder(BackendStrategy::Direct);

            builder.propagate_headers_names(
                run.propagate_headers
                    .into_iter()
                    .filter_map(|h| h.parse().ok())
                    .collect(),
            );

            let backend = builder.build(backend_connector);
            let mut secrets = vec![];
            if let Some(secret) = run.secret {
                for (name, s) in secret.into_iter() {
                    secrets.push(runtime::app::Secret {
                        name,
                        secret_values: vec![SecretValue {
                            effective_from: 0,
                            value: s.to_string(),
                        }],
                    });
                }
            }

            let cli_app = App {
                binary_id: 0,
                max_duration: run.max_duration.map(|m| m / 10).unwrap_or(60000),
                mem_limit: run.mem_limit.unwrap_or((128 * MB) as usize),
                env: run.env.unwrap_or_default().into_iter().collect(),
                rsp_headers: Default::default(),
                log: Default::default(),
                app_id: 0,
                client_id: 0,
                plan: SmolStr::new("cli"),
                status: Status::Enabled,
                debug_until: None,
                secrets,
            };

            let mut headers: HashMap<SmolStr, SmolStr> =
                run.headers.unwrap_or_default().into_iter().collect();

            append_headers(run.geo, &mut headers);

            let context = CliContext {
                headers,
                engine,
                app: Some(cli_app),
                backend,
                wasm_bytes,
                wasi_http: run.wasi_http.unwrap_or_default(),
            };

            let http: HttpService<CliContext, SecretImpl> = ServiceBuilder::new(context).build()?;
            let http = http.run(HttpConfig {
                all_interfaces: false,
                port: run.port,
                #[cfg(target_family = "unix")]
                cancel: shutdown_coordinator.handle_weak(),
                listen_fd: None,
                backoff: 64,
            });
            let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;
            tokio::select! {
                res = http => {
                    res?
                },
                _ = signal::ctrl_c() => {
                    #[cfg(target_family = "unix")]
                    shutdown_coordinator.shutdown().await
                }
                _ = terminate.recv() => {
                    #[cfg(target_family = "unix")]
                    shutdown_coordinator.shutdown().await
                }
            }
        }
    };
    Ok(())
}

/// Append to request headers:
/// * server_name if it is missing
/// * Gcore PoP sample Geo headers
fn append_headers(geo: bool, headers: &mut HashMap<SmolStr, SmolStr>) {
    if !headers
        .keys()
        .any(|k| "server_name".eq_ignore_ascii_case(k))
    {
        headers.insert("server_name".to_smolstr(), "test.localhost".to_smolstr());
    }

    if geo {
        headers.insert("pop-lat".to_smolstr(), "49.6113".to_smolstr());
        headers.insert("pop-long".to_smolstr(), "6.1294".to_smolstr());
        headers.insert("pop-reg".to_smolstr(), "lu".to_smolstr());
        headers.insert("pop-city".to_smolstr(), "luxembourg".to_smolstr());
        headers.insert("pop-continent".to_smolstr(), "eu".to_smolstr());
        headers.insert("pop-country-code".to_smolstr(), "au".to_smolstr());
        headers.insert("pop-country-name".to_smolstr(), "luxembourg".to_smolstr());
    }
}

impl PreCompiledLoader<u64> for CliContext {
    fn load_component(&self, _id: u64) -> anyhow::Result<Component> {
        let wasm_sample = componentize_if_necessary(&self.wasm_bytes)?;
        Component::new(&self.engine, wasm_sample)
    }

    fn load_module(&self, _id: u64) -> anyhow::Result<Module> {
        unreachable!("")
    }
}

impl ContextT for CliContext {
    type BackendConnector = HttpsConnector<HttpConnector>;
    fn make_logger(&self, _app_name: SmolStr, _wrk: &App) -> Logger {
        Logger::new(Console::default())
    }

    fn backend(&self) -> Backend<HttpsConnector<HttpConnector>> {
        self.backend.to_owned()
    }

    fn loader(&self) -> &dyn PreCompiledLoader<u64> {
        self
    }

    fn engine_ref(&self) -> &Engine {
        &self.engine
    }
}

enum CliExecutor {
    Http(HttpExecutorImpl<HttpsConnector<HttpConnector>, SecretImpl>),
    Wasi(WasiHttpExecutorImpl<HttpsConnector<HttpConnector>, SecretImpl>),
}

#[async_trait]
impl HttpExecutor for CliExecutor {
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
        match self {
            CliExecutor::Http(ref executor) => executor.execute(req, on_response).await,
            CliExecutor::Wasi(ref executor) => executor.execute(req, on_response).await,
        }
    }
}

impl ExecutorFactory<HttpState<HttpsConnector<HttpConnector>, SecretImpl>> for CliContext {
    type Executor = CliExecutor;

    fn get_executor(
        &self,
        name: SmolStr,
        app: &App,
        engine: &WasmEngine<HttpState<HttpsConnector<HttpConnector>, SecretImpl>>,
    ) -> anyhow::Result<Self::Executor> {
        let mut dictionary = Dictionary::new();
        for (k, v) in app.env.iter() {
            dictionary.insert(k.to_string(), v.to_string());
        }
        let mut secret_impl = SecretImpl::default();
        for s in app.secrets.iter() {
            if let Some(value) = s.secret_values.first() {
                secret_impl.insert(s.name.to_string(), value.value.to_string());
            }
        }
        let secret = Secret::new(secret_impl);

        let env = app.env.iter().collect::<Vec<(&SmolStr, &SmolStr)>>();

        let logger = self.make_logger(name, app);

        let version = WasiVersion::Preview2;
        let store_builder = engine
            .store_builder(version)
            .set_env(&env)
            .max_memory_size(app.mem_limit)
            .max_epoch_ticks(app.max_duration)
            .logger(logger);

        let component = self.loader().load_component(app.binary_id)?;
        let instance_pre = engine.component_instantiate_pre(&component)?;
        if self.wasi_http {
            Ok(CliExecutor::Wasi(WasiHttpExecutorImpl::new(
                instance_pre,
                store_builder,
                self.backend(),
                dictionary,
                secret,
            )))
        } else {
            Ok(CliExecutor::Http(HttpExecutorImpl::new(
                instance_pre,
                store_builder,
                self.backend(),
                dictionary,
                secret,
            )))
        }
    }
}

impl ExecutorCache for CliContext {
    fn remove(&self, _name: &str) {
        unreachable!()
    }

    fn remove_all(&self) {
        unreachable!()
    }
}

impl ContextHeaders for CliContext {
    fn append_headers(&self) -> impl Iterator<Item = (SmolStr, SmolStr)> {
        self.headers.iter().map(|(k, v)| (k.clone(), v.clone()))
    }
}

impl Router for CliContext {
    async fn lookup_by_name(&self, _name: &str) -> Option<App> {
        self.app.to_owned()
    }

    async fn lookup_by_id(&self, _id: u64) -> Option<(SmolStr, App)> {
        unreachable!()
    }
}

impl StatsWriter for CliContext {
    fn write_stats(&self, _stat: StatRow) {}
}

fn parse_key_value<T, U>(
    s: &str,
) -> Result<(T, U), Box<dyn std::error::Error + Send + Sync + 'static>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
    U: std::str::FromStr,
    U::Err: std::error::Error + Send + Sync + 'static,
{
    let pos = s
        .find('=')
        .ok_or_else(|| format!("invalid KEY=value: no `=` found in `{s}`"))?;
    Ok((s[..pos].parse()?, s[pos + 1..].parse()?))
}
