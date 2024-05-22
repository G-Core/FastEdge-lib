use std::time::{Duration, Instant};

use anyhow::bail;
use async_trait::async_trait;
use bytesize::ByteSize;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use tracing::error;
use wasmtime_wasi_http::WasiHttpView;

use runtime::store::StoreBuilder;
use runtime::InstancePre;

use crate::executor::HttpExecutor;
use crate::HttpState;

/// Execute context used by ['HttpService']
#[derive(Clone)]
pub struct WasiHttpExecutorImpl {
    instance_pre: InstancePre<HttpState>,
    store_builder: StoreBuilder,
}

#[async_trait]
impl HttpExecutor for WasiHttpExecutorImpl {
    async fn execute(
        &self,
        req: hyper::Request<Incoming>,
    ) -> anyhow::Result<(hyper::Response<Full<Bytes>>, Duration, ByteSize)> {
        let start_ = Instant::now();
        let response = self.execute_impl(req).await;
        let elapsed = Instant::now().duration_since(start_);
        response.map(|(r, used)| (r, elapsed, used))
    }
}

impl WasiHttpExecutorImpl {
    pub fn new(instance_pre: InstancePre<HttpState>, store_builder: StoreBuilder) -> Self {
        Self {
            instance_pre,
            store_builder,
        }
    }

    async fn execute_impl(
        &self,
        req: hyper::Request<Incoming>,
    ) -> anyhow::Result<(hyper::Response<Full<Bytes>>, ByteSize)> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let (parts, body) = req.into_parts();

        let body = body.collect().await.expect("incoming body").to_bytes();
        let body = Full::new(body).map_err(|never| match never {});
        let body = body.boxed();

        let store_builder = self.store_builder.to_owned(); //.with_properties(properties);
        let wasi_nn = self
            .store_builder
            .make_wasi_nn_ctx()
            .expect("make_wasi_nn_ctx");

        let state = HttpState { wasi_nn };

        let mut store = store_builder.build(state).expect("store build");
        let instance_pre = self.instance_pre.clone();

        let task = tokio::task::spawn(async move {
            let req = store
                .data_mut()
                .new_incoming_request(hyper::Request::from_parts(parts, body))
                .expect("new incoming request");
            let out = store
                .data_mut()
                .new_response_outparam(sender)
                .expect("new response outparam");

            let (proxy, _inst) = wasmtime_wasi_http::proxy::Proxy::instantiate_pre(
                &mut store,
                instance_pre.as_ref(),
            )
            .await?;

            if let Err(e) = proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, req, out)
                .await
            {
                error!(cause=?e, "incoming handler");
                return Err(e);
            };
            let used = ByteSize::b(store.memory_used() as u64);
            Ok(used)
        });

        match receiver.await {
            Ok(Ok(resp)) => {
                let (parts, body) = resp.into_parts();
                let body = body.collect().await.expect("incoming body").to_bytes();
                let body = Full::new(body);
                let used = task.await.expect("task await").expect("byte size");
                Ok((hyper::Response::from_parts(parts, body), used))
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                // An error in the receiver (`RecvError`) only indicates that the
                // task exited before a response was sent (i.e., the sender was
                // dropped); it does not describe the underlying cause of failure.
                // Instead we retrieve and propagate the error from inside the task
                // which should more clearly tell the user what went wrong. Note
                // that we assume the task has already exited at this point so the
                // `await` should resolve immediately.
                let e = match task.await {
                    Ok(r) => {
                        r.expect_err("if the receiver has an error, the task must have failed")
                    }
                    Err(e) => e.into(),
                };
                bail!("guest never invoked `response-outparam::set` method: {e:?}")
            }
        }
    }
}
