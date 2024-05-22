mod wasi_http;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Error, Result};
use async_trait::async_trait;
use bytesize::ByteSize;
use http::{HeaderMap, HeaderValue, Method};
use http_backend::Backend;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use smol_str::SmolStr;
use wasmtime_wasi::StdoutStream;

use reactor::gcore::fastedge;
use runtime::store::StoreBuilder;
use runtime::{App, InstancePre, WasmEngine};

use crate::HttpState;

pub use wasi_http::WasiHttpExecutorImpl;

pub(crate) static X_REAL_IP: &str = "x-real-ip";
pub(crate) static TRACEPARENT: &str = "traceparent";
pub(crate) static X_CDN_REQUESTOR: &str = "x-cdn-requestor";

#[async_trait]
pub trait HttpExecutor {
    async fn execute(
        &self,
        req: hyper::Request<Incoming>,
    ) -> Result<(hyper::Response<Full<Bytes>>, Duration, ByteSize)>;
}

pub trait ExecutorFactory<C> {
    type Executor;
    fn get_executor(
        &self,
        name: SmolStr,
        app: &App,
        engine: &WasmEngine<C>,
    ) -> Result<Self::Executor>;
}

/// Execute context used by ['HttpService']
#[derive(Clone)]
pub struct HttpExecutorImpl<C> {
    instance_pre: InstancePre<HttpState>,
    store_builder: StoreBuilder,
    backend: Backend<C>,
}

#[async_trait]
impl<C> HttpExecutor for HttpExecutorImpl<C>
where
    C: Clone + Send + Sync + 'static,
{
    async fn execute(
        &self,
        req: hyper::Request<Incoming>,
    ) -> Result<(hyper::Response<Full<Bytes>>, Duration, ByteSize)> {
        let start_ = Instant::now();
        let response = self.execute_impl(req).await;
        let elapsed = Instant::now().duration_since(start_);
        response.map(|(r, used)| (r, elapsed, used))
    }
}

impl<C> HttpExecutorImpl<C>
where
    C: Clone + Send + Sync + 'static,
{
    pub fn new(
        instance_pre: InstancePre<HttpState>,
        store_builder: StoreBuilder,
        backend: Backend<C>,
    ) -> Self {
        Self {
            instance_pre,
            store_builder,
            backend,
        }
    }

    async fn execute_impl(
        &self,
        req: hyper::Request<Incoming>,
    ) -> Result<(hyper::Response<Full<Bytes>>, ByteSize)> {
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

        let body = body.collect().await?.to_bytes();
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

        let properties = Self::get_properties(&parts.headers);

        let store_builder = self.store_builder.to_owned().with_properties(properties);
        let wasi_nn = self.store_builder.make_wasi_nn_ctx()?;
        let mut http_backend = self.backend.to_owned();

        http_backend
            .propagate_headers(&parts.headers)
            .context("propagate headers")?;

        let state = HttpState { wasi_nn };

        let mut store = store_builder.build(state)?;

        let instance = self.instance_pre.instantiate_async(&mut store).await?;
        let func = instance
            .exports(&mut store)
            .instance("gcore:fastedge/http-handler")
            .ok_or_else(|| anyhow!("gcore:fastedge/http-handler instance not found"))?
            .typed_func::<(fastedge::http::Request,), (fastedge::http::Response,)>("process")?;
        let (resp,) = match func.call_async(&mut store, (request,)).await {
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
        let builder =
            ::http::Response::builder().status(::http::StatusCode::try_from(resp.status)?);
        let builder = if let Some(headers) = resp.headers {
            headers
                .iter()
                .fold(builder, |builder, (k, v)| builder.header(k, v))
        } else {
            builder
        };
        let used = ByteSize::b(store.memory_used() as u64);

        let body = resp.body.map(|b| Full::from(b)).unwrap_or_default();
        builder.body(body).map(|r| (r, used)).map_err(Error::msg)
    }

    fn get_properties(headers: &HeaderMap<HeaderValue>) -> HashMap<String, String> {
        let mut properties = HashMap::new();
        if let Some(client_ip) = headers.get(X_REAL_IP).and_then(|v| v.to_str().ok()) {
            properties.insert("client_ip".to_owned(), client_ip.to_owned());
        }
        if let Some(traceparent) = headers.get(TRACEPARENT).and_then(|v| v.to_str().ok()) {
            properties.insert("traceparent".to_owned(), traceparent.to_owned());
        }
        if let Some(requestor) = headers.get(X_CDN_REQUESTOR).and_then(|v| v.to_str().ok()) {
            properties.insert("requestor".to_owned(), requestor.to_owned());
        }
        properties
    }
}

fn to_fastedge_http_method(method: &Method) -> Result<fastedge::http::Method> {
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


