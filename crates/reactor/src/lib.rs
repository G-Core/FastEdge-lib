#![allow(missing_docs)]

wasmtime::component::bindgen!({
    path: "../../sdk/wit",
    world: "reactor",
    async: true,
    tracing: false,
});
