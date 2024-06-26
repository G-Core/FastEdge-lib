use clap::{Args, Parser, Subcommand};
use http_backend::{Backend, BackendStrategy};
use http_service::executor::{ExecutorFactory, HttpExecutorImpl};
use http_service::{ContextHeaders, HttpConfig, HttpService, HttpState};
use hyper::client::HttpConnector;
use hyper_tls::HttpsConnector;
use runtime::app::Status;
use runtime::logger::{Console, Logger};
use runtime::service::{Service, ServiceBuilder};
use runtime::{
    componentize_if_necessary, App, ContextT, ExecutorCache, PreCompiledLoader, Router,
    WasiVersion, WasmConfig, WasmEngine,
};
use smol_str::{SmolStr, ToSmolStr};
use std::collections::HashMap;
use std::path::PathBuf;
use shellflip::ShutdownCoordinator;
use wasmtime::component::Component;
use wasmtime::{Engine, Module};
use runtime::util::stats::{StatRow, StatsWriter};

#[derive(Debug, Parser)]
#[command(name = "cli")]
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
    #[arg(long, value_parser = parse_key_value::< String, String >)]
    envs: Option<Vec<(SmolStr, SmolStr)>>,
    /// Add header from original request
    #[arg(long = "propagate-header", num_args = 0..)]
    propagate_headers: Option<Vec<String>>,
    /// Execution context headers added to request
    #[arg(long, value_parser = parse_key_value::< String, String >)]
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
}

/// Test tool execution context
struct CliContext {
    headers: HashMap<SmolStr, SmolStr>,
    engine: Engine,
    app: Option<App>,
    backend: Backend<HttpsConnector<HttpConnector>>,
    wasm_bytes: Vec<u8>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();
    let shutdown_coordinator = ShutdownCoordinator::new();
    let args = Cli::parse();
    let config = WasmConfig::default();
    let engine = Engine::new(&config)?;

    match args.command {
        Commands::Http(run) => {
            let wasm_bytes = tokio::fs::read(&run.wasm).await?;

            let backend_connector = HttpsConnector::new();
            let mut builder =
                Backend::<HttpsConnector<HttpConnector>>::builder(BackendStrategy::Direct);
            if let Some(propagate_headers) = run.propagate_headers {
                builder.propagate_headers_names(propagate_headers);
            }
            let backend = builder.build(backend_connector);
            let cli_app = App {
                binary_id: 0,
                max_duration: run.max_duration.map(|m| m / 10).unwrap_or(60000),
                mem_limit: run.mem_limit.unwrap_or(u32::MAX as usize),
                env: run.envs.unwrap_or_default().into_iter().collect(),
                rsp_headers: Default::default(),
                log: Default::default(),
                app_id: 0,
                client_id: 0,
                plan: SmolStr::new("cli"),
                status: Status::Enabled,
                debug_until: None,
            };

            let mut headers: HashMap<SmolStr, SmolStr> = run
                .headers
                .unwrap_or_default()
                .into_iter()
                .collect();

            append_headers(run.geo, &mut headers);

            let context = CliContext {
                headers,
                engine,
                app: Some(cli_app),
                backend,
                wasm_bytes,
            };

            let http: HttpService<CliContext> = ServiceBuilder::new(context).build()?;
            let http = http.run(HttpConfig {
                all_interfaces: false,
                port: run.port,
                cancel: shutdown_coordinator.handle_weak(),
                listen_fd: None,
            });
            tokio::select! {
                _ = http => {

                },
                _ = tokio::signal::ctrl_c() => {
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
        headers.insert(
            "Server_name".to_smolstr(),
            "test.localhost".to_smolstr(),
        );
    }

    if geo {
        headers.insert("PoP-Lat".to_smolstr(),"49.6113".to_smolstr());
        headers.insert("PoP-Long".to_smolstr(), "6.1294".to_smolstr());
        headers.insert("PoP-Reg".to_smolstr(), "LU".to_smolstr());
        headers.insert("PoP-City".to_smolstr(), "Luxembourg".to_smolstr());
        headers.insert("PoP-Continent".to_smolstr(), "EU".to_smolstr());
        headers.insert("PoP-Country-Code".to_smolstr(), "AU".to_smolstr());
        headers.insert(
            "PoP-Country-Name".to_smolstr(),
            "Luxembourg".to_smolstr(),
        );
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

impl ExecutorFactory<HttpState<HttpsConnector<HttpConnector>>> for CliContext {
    type Executor = HttpExecutorImpl<HttpsConnector<HttpConnector>>;

    fn get_executor(
        &self,
        name: SmolStr,
        app: &App,
        engine: &WasmEngine<HttpState<HttpsConnector<HttpConnector>>>,
    ) -> anyhow::Result<Self::Executor> {
        let env = app
            .env
            .iter()
            .collect::<Vec<(&SmolStr, &SmolStr)>>();

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
        Ok(HttpExecutorImpl::new(
            instance_pre,
            store_builder,
            self.backend(),
        ))
    }
}

impl ExecutorCache for CliContext {
    fn invalidate(&self, _name: &str) {
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
    async fn write_stats(&self, _stat: StatRow) {}
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
