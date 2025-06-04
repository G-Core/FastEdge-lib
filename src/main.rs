mod context;
mod dotenv;
mod executor;
mod key_value;
mod secret;

use bytesize::MB;
use clap::{Args, Parser, Subcommand};
use context::Context;
use dotenv::{DotEnvInjector, EnvArgType};
use http_backend::{Backend, BackendStrategy};
use http_service::{HttpConfig, HttpService};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use runtime::app::Status;
use runtime::service::{Service, ServiceBuilder};
use runtime::{App, SecretValue, WasmConfig};
use smol_str::{SmolStr, ToSmolStr};
use std::collections::HashMap;
use std::path::PathBuf;
use wasmtime::Engine;

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
    /// Dotenv file path
    #[arg(long, num_args = 0..=1)]
    dotenv: Option<Option<PathBuf>>,
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

            let has_dotenv_flag = run.dotenv.is_some();

            let dotenv_injector = if let Some(dotenv_path) = run.dotenv.flatten() {
                DotEnvInjector::new(Some(dotenv_path))
            } else {
                DotEnvInjector::new(None)
            };

            let env = dotenv_injector.merge_with_dotenv_variables(
                has_dotenv_flag,
                EnvArgType::Env,
                run.env.unwrap_or_default().into_iter().collect(),
            );

            let secret_args = dotenv_injector.merge_with_dotenv_variables(
                has_dotenv_flag,
                EnvArgType::Secrets,
                run.secret.unwrap_or_default().into_iter().collect(),
            );

            let mut secrets = vec![];
            for (name, s) in secret_args {
                secrets.push(runtime::app::SecretOption {
                    name,
                    secret_values: vec![SecretValue {
                        effective_from: 0,
                        value: s.to_string(),
                    }],
                });
            }

            let cli_app = App {
                binary_id: 0,
                max_duration: run.max_duration.map(|m| m / 10).unwrap_or(60000),
                mem_limit: run.mem_limit.unwrap_or((128 * MB) as usize),
                // env: run.env.unwrap_or_default().into_iter().collect(),
                env,
                rsp_headers: Default::default(),
                log: Default::default(),
                app_id: 0,
                client_id: 0,
                plan: SmolStr::new("cli"),
                status: Status::Enabled,
                debug_until: None,
                secrets,
                kv_stores: vec![],
            };

            // let mut headers: HashMap<SmolStr, SmolStr> =
            //     run.headers.unwrap_or_default().into_iter().collect();

            let mut headers = dotenv_injector.merge_with_dotenv_variables(
                has_dotenv_flag,
                EnvArgType::RspHeader,
                run.headers.unwrap_or_default().into_iter().collect(),
            );

            append_headers(run.geo, &mut headers);

            let context = Context {
                headers,
                engine,
                app: Some(cli_app),
                backend,
                wasm_bytes,
                wasi_http: run.wasi_http.unwrap_or_default(),
            };

            let http: HttpService<Context> = ServiceBuilder::new(context).build()?;
            let http = http.run(HttpConfig {
                all_interfaces: false,
                port: run.port,
                #[cfg(target_family = "unix")]
                cancel: shutdown_coordinator.handle_weak(),
                listen_fd: None,
                backoff: 64,
            });
            #[cfg(target_family = "unix")]
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

            #[cfg(target_family = "windows")]
            let mut terminate = tokio::signal::windows::ctrl_close()?;

            tokio::select! {
                res = http => {
                    res?
                },
                _ = tokio::signal::ctrl_c() => {
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
