//! Rebuilds the crate when a migration is added or changed.
//!
//! `sqlx::migrate!` embeds the migration files at compile time, but cargo only
//! knows to recompile when it is told the directory is an input.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
