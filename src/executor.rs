use async_trait::async_trait;
use bytesize::ByteSize;
use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use http_service::executor::{HttpExecutor, HttpExecutorImpl, WasiHttpExecutorImpl};
use http_service::HyperOutgoingBody;
use hyper::body::Body;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use std::time::Duration;
use crate::key_value::CliStoreManager;

pub enum RunExecutor {
    Http(HttpExecutorImpl<HttpsConnector<HttpConnector>, CliStoreManager>),
    Wasi(WasiHttpExecutorImpl<HttpsConnector<HttpConnector>, CliStoreManager>),
}

#[async_trait]
impl HttpExecutor for RunExecutor {
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
            RunExecutor::Http(ref executor) => executor.execute(req, on_response).await,
            RunExecutor::Wasi(ref executor) => executor.execute(req, on_response).await,
        }
    }
}
