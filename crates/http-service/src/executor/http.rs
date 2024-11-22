use crate::executor;
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

        let properties = executor::get_properties(&parts.headers);

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
