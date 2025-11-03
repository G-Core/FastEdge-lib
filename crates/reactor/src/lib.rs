#![allow(missing_docs)]

wasmtime::component::bindgen!({
    path: "wit",
    world: "reactor",
    imports: { default: async },
});
