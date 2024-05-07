use crate::service::Service;
use crate::WasmEngine;
use tokio_util::sync::CancellationToken;

pub struct StubService;

impl Service for StubService {
    type State = ();
    type Config = CancellationToken;
    type Context = ();

    fn new(_engine: WasmEngine<Self::State>, _context: Self::Context) -> anyhow::Result<Self> {
        Ok(Self)
    }

    async fn run(self, config: Self::Config) -> anyhow::Result<()> {
        config.cancelled().await;
        Ok(())
    }
}
