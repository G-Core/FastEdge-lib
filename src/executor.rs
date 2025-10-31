use async_trait::async_trait;
use http::{Request, Response};
use http_body_util::BodyExt;
use http_service::executor::{HttpExecutor, HttpExecutorImpl, WasiHttpExecutorImpl};
use http_service::HyperOutgoingBody;
use hyper::body::Body;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use runtime::util::stats::StatsVisitor;
use std::sync::Arc;

pub enum RunExecutor {
    Http(HttpExecutorImpl<HttpsConnector<HttpConnector>>),
    Wasi(WasiHttpExecutorImpl<HttpsConnector<HttpConnector>>),
}

#[async_trait]
impl HttpExecutor for RunExecutor {
    async fn execute<B>(
        &self,
        req: Request<B>,
        stats: Arc<dyn StatsVisitor>,
    ) -> anyhow::Result<Response<HyperOutgoingBody>>
    where
        B: BodyExt + Send,
        <B as Body>::Data: Send,
    {
        match self {
            RunExecutor::Http(ref executor) => executor.execute(req, stats).await,
            RunExecutor::Wasi(ref executor) => executor.execute(req, stats).await,
        }
    }
}
