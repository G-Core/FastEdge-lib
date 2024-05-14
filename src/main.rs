use clap::{Args, Parser, Subcommand};
use http_backend::{Backend, BackendStrategy};
use http_service::executor::{ExecutorFactory, HttpExecutorImpl};
use http_service::stats::{StatRow, StatsWriter};
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
use smol_str::SmolStr;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use wasmtime::component::Component;
use wasmtime::{Engine, Module};

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
    envs: Option<Vec<(String, String)>>,
    /// Add header from original request
    #[arg(long = "propagate-header", num_args = 0..)]
    propagate_headers: Option<Vec<String>>,
    /// Execution context headers added to request
    #[arg(long, value_parser = parse_key_value::< String, String >)]
    headers: Option<Vec<(String, String)>>,
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
struct CliContext<'a> {
    headers: HashMap<Cow<'a, str>, Cow<'a, str>>,
    engine: Engine,
    app: Option<App>,
    backend: Backend<HttpsConnector<HttpConnector>>,
    wasm_bytes: Vec<u8>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();
    let cancel = CancellationToken::new();
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
                max_duration: run.max_duration.map(|m| m / 10).unwrap_or(u64::MAX),
                mem_limit: run.mem_limit.unwrap_or(u32::MAX as usize),
                env: run.envs.unwrap_or_default().into_iter().collect(),
                rsp_headers: Default::default(),
                log: Default::default(),
                app_id: 0,
                client_id: 0,
                plan: "cli".to_string(),
                status: Status::Enabled,
                debug_until: None,
            };

            let mut headers: HashMap<Cow<str>, Cow<str>> = run
                .headers
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
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
                https: None,
                cancel: cancel.clone(),
            });
            tokio::select! {
                _ = http => {

                },
                _ = tokio::signal::ctrl_c() => {
                    cancel.cancel()
                }
            }
        }
    };
    Ok(())
}

/// Append to request headers:
/// * server_name if it is missing
/// * Gcore PoP sample Geo headers
fn append_headers(geo: bool, headers: &mut HashMap<Cow<str>, Cow<str>>) {
    if !headers
        .keys()
        .any(|k| "server_name".eq_ignore_ascii_case(k))
    {
        headers.insert(
            Cow::Borrowed("Server_name"),
            Cow::Borrowed("test.localhost"),
        );
    }

    if geo {
        headers.insert(Cow::Borrowed("PoP-Lat"), Cow::Borrowed("49.6113"));
        headers.insert(Cow::Borrowed("PoP-Long"), Cow::Borrowed("6.1294"));
        headers.insert(Cow::Borrowed("PoP-Reg"), Cow::Borrowed("LU"));
        headers.insert(Cow::Borrowed("PoP-City"), Cow::Borrowed("Luxembourg"));
        headers.insert(Cow::Borrowed("PoP-Continent"), Cow::Borrowed("EU"));
        headers.insert(Cow::Borrowed("PoP-Country-Code"), Cow::Borrowed("AU"));
        headers.insert(
            Cow::Borrowed("PoP-Country-Name"),
            Cow::Borrowed("Luxembourg"),
        );
    }
}

impl PreCompiledLoader<u64> for CliContext<'_> {
    fn load_component(&self, _id: u64) -> anyhow::Result<Component> {
        let wasm_sample = componentize_if_necessary(&self.wasm_bytes)?;
        Component::new(&self.engine, wasm_sample)
    }

    fn load_module(&self, _id: u64) -> anyhow::Result<Module> {
        unreachable!("")
    }
}

impl ContextT for CliContext<'_> {
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

impl ExecutorFactory<HttpState<HttpsConnector<HttpConnector>>> for CliContext<'_> {
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
            .collect::<Vec<(&String, &String)>>();

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

impl ExecutorCache for CliContext<'_> {
    fn invalidate(&self, _name: &str) {
        unreachable!()
    }
}

impl ContextHeaders for CliContext<'_> {
    fn append_headers(&self) -> impl Iterator<Item = (Cow<str>, Cow<str>)> {
        self.headers.iter().map(|(k, v)| (k.clone(), v.clone()))
    }
}

impl Router for CliContext<'_> {
    async fn lookup(&self, _name: &str) -> Option<App> {
        self.app.to_owned()
    }
}

impl StatsWriter for CliContext<'_> {
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
