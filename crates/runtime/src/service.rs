use crate::{ContextT, WasmEngine, WasmEngineBuilder};
use std::marker::PhantomData;

pub trait Service: Sized + Send + Sync {
    type State;
    type Config;
    type Context;

    /// Create a new service.
    fn new(engine: WasmEngine<Self::State>, context: Self::Context) -> anyhow::Result<Self>;

    /// Run the service.
    fn run(
        self,
        config: Self::Config,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Make changes to the ExecutionContext using the given Builder.
    fn configure_engine(_builder: &mut WasmEngineBuilder<Self::State>) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct ServiceBuilder<C, S> {
    context: C,
    _phantom: PhantomData<S>,
}

impl<C, S: Service<Context = C>> ServiceBuilder<C, S>
where
    C: ContextT,
    S: Service<Context = C>,
    <S as Service>::State: Send + Sync + 'static,
{
    /// Create a new ServiceBuilder.
    pub fn new(context: C) -> Self {
        Self {
            context,
            _phantom: PhantomData,
        }
    }

    pub fn build(self) -> anyhow::Result<S> {
        let engine = {
            let mut builder = WasmEngine::builder(self.context.engine_ref())?;
            S::configure_engine(&mut builder)?;
            builder.build()
        };

        // create new Service
        S::new(engine, self.context)
    }
}
