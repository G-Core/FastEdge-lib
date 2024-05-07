#![allow(missing_docs)]

wasmtime::component::bindgen!({
    path: "../../sdk/wit",
    world: "http-reactor",
    async: true,
    tracing: false,
});
