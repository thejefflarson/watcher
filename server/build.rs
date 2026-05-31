//! rust-embed needs the UI asset folder to exist at compile time. In a full
//! image/CI build the UI is built into `../ui/dist` first; for a bare
//! `cargo build`/`cargo test` (no UI), create an empty placeholder so the
//! macro expands. An empty dir just means the server serves a "UI not built"
//! notice — the API still works.
use std::path::Path;

fn main() {
    let dist = Path::new("../ui/dist");
    if !dist.exists() {
        std::fs::create_dir_all(dist).ok();
    }
    println!("cargo:rerun-if-changed=../ui/dist");
}
