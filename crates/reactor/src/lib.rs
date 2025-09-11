#![allow(missing_docs)]

wasmtime::component::bindgen!({
    path: "../../sdk/wit",
    world: "reactor",
    imports: { default: async },
});
