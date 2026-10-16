//! Generated bindings for the plugin interfaces and provider worlds.
//!
//! The bindings are generated in two layers. [`imports`] owns the host imports
//! every provider world shares (`types` and the `host-*` interfaces), so the
//! host implements each import exactly once. Each provider world then remaps its
//! imports to that layer with wasmtime's `with` option, which is what lets one
//! linker and one [`InstanceState`](crate::state::InstanceState) serve every
//! world.
//!
//! This module is produced by wasmtime from the WIT package in `wit/`; it is not
//! written by hand.

#![allow(missing_docs, unsafe_code, clippy::all)]

/// The host imports every provider world shares.
///
/// The world lists all of them even though a given plugin imports only the ones
/// it calls, so linking this world provides every import the host offers and no
/// provider world needs its own `add_to_linker`.
pub(crate) mod imports {
    wasmtime::component::bindgen!({
        path: "../../wit",
        inline: "
            package hearthsound:host@0.2.0;

            world host-imports {
                import hearthsound:plugin/types@0.2.0;
                import hearthsound:plugin/host-log@0.2.0;
                import hearthsound:plugin/host-http@0.2.0;
                import hearthsound:plugin/host-kv@0.2.0;
                import hearthsound:plugin/host-config@0.2.0;
                import hearthsound:plugin/host-files@0.2.0;
            }
        ",
        world: "hearthsound:host/host-imports",
        imports: { default: async | trappable },
    });
}

/// The catalog provider world.
pub(crate) mod catalog {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "hearthsound:plugin/catalog-provider",
        imports: { default: async | trappable },
        exports: { default: async },
        with: {
            "hearthsound:plugin/types@0.2.0": crate::bindings::imports::hearthsound::plugin::types,
            "hearthsound:plugin/host-log@0.2.0": crate::bindings::imports::hearthsound::plugin::host_log,
            "hearthsound:plugin/host-http@0.2.0": crate::bindings::imports::hearthsound::plugin::host_http,
            "hearthsound:plugin/host-kv@0.2.0": crate::bindings::imports::hearthsound::plugin::host_kv,
            "hearthsound:plugin/host-config@0.2.0": crate::bindings::imports::hearthsound::plugin::host_config,
        },
    });
}

/// The metadata provider world.
pub(crate) mod metadata {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "hearthsound:plugin/metadata-provider",
        imports: { default: async | trappable },
        exports: { default: async },
        with: {
            "hearthsound:plugin/types@0.2.0": crate::bindings::imports::hearthsound::plugin::types,
            "hearthsound:plugin/host-log@0.2.0": crate::bindings::imports::hearthsound::plugin::host_log,
            "hearthsound:plugin/host-http@0.2.0": crate::bindings::imports::hearthsound::plugin::host_http,
            "hearthsound:plugin/host-kv@0.2.0": crate::bindings::imports::hearthsound::plugin::host_kv,
            "hearthsound:plugin/host-config@0.2.0": crate::bindings::imports::hearthsound::plugin::host_config,
        },
    });
}

/// The acquisition provider world.
pub(crate) mod acquisition {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "hearthsound:plugin/acquisition-provider",
        imports: { default: async | trappable },
        exports: { default: async },
        with: {
            "hearthsound:plugin/types@0.2.0": crate::bindings::imports::hearthsound::plugin::types,
            "hearthsound:plugin/host-log@0.2.0": crate::bindings::imports::hearthsound::plugin::host_log,
            "hearthsound:plugin/host-http@0.2.0": crate::bindings::imports::hearthsound::plugin::host_http,
            "hearthsound:plugin/host-kv@0.2.0": crate::bindings::imports::hearthsound::plugin::host_kv,
            "hearthsound:plugin/host-config@0.2.0": crate::bindings::imports::hearthsound::plugin::host_config,
            "hearthsound:plugin/host-files@0.2.0": crate::bindings::imports::hearthsound::plugin::host_files,
        },
    });
}

pub(crate) use acquisition::{AcquisitionProvider, AcquisitionProviderPre};
pub(crate) use catalog::{CatalogProvider, CatalogProviderPre};
pub(crate) use metadata::{MetadataProvider, MetadataProviderPre};

/// The `hearthsound:plugin` interface tree of the host import layer.
pub(crate) use imports::hearthsound;

/// The exported interface tree of the catalog provider world.
pub(crate) use catalog::exports;
