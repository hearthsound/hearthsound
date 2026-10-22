//! Build script of the server crate.
//!
//! The pinned web client is embedded from `target/web-client/` at the
//! repository root (see `web-client.lock` and `cargo xtask fetch-web-client`).
//! rust-embed walks the folder at compile time, so a fetch, or its removal,
//! must rebuild the crate instead of shipping a client the lock file no
//! longer pins, or none where one is pinned.

fn main() {
    println!("cargo:rerun-if-changed=../../target/web-client");
}
