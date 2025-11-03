mod http;
mod wasi_http;

use runtime::util::stats::StatsVisitor;
use std::collections::HashMap;
use std::sync::Arc;

use ::http::{HeaderMap, HeaderValue};
use anyhow::Result;
use async_trait::async_trait;
use http_body_util::BodyExt;
use hyper::body::Body;
use runtime::{App, WasmEngine};
use smol_str::SmolStr;
use wasmtime_wasi_http::body::HyperOutgoingBody;

pub use http::HttpExecutorImpl;
pub use wasi_http::WasiHttpExecutorImpl;

pub(crate) static X_REAL_IP: &str = "x-real-ip";
pub(crate) static TRACEPARENT: &str = "traceparent";
pub(crate) static X_CDN_REQUESTOR: &str = "x-cdn-requestor";

#[async_trait]
pub trait HttpExecutor {
    async fn execute<B>(
        self,
        req: hyper::Request<B>,
        stats: Arc<dyn StatsVisitor>,
    ) -> Result<hyper::Response<HyperOutgoingBody>>
    where
        B: BodyExt + Send,
        <B as Body>::Data: Send;
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

pub(crate) fn get_properties(headers: &HeaderMap<HeaderValue>) -> HashMap<String, String> {
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
