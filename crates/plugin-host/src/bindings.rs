//! Generated bindings for the catalog provider world.
//!
//! This module is produced by wasmtime from the WIT package in `wit/`; it is
//! not written by hand. It carries the guest export traits the host calls and
//! the host import traits the host implements.

#![allow(missing_docs, unsafe_code, clippy::all)]

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "hearthsound:plugin/catalog-provider",
    imports: { default: async | trappable },
    exports: { default: async },
});
