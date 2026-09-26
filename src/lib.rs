//! Shared browser entry point. The desktop binary keeps its existing startup path.
#[cfg(target_arch = "wasm32")]
mod export;
#[cfg(target_arch = "wasm32")]
mod filter;
#[cfg(target_arch = "wasm32")]
mod gfa;
#[cfg(target_arch = "wasm32")]
mod gpu_layout;
#[cfg(target_arch = "wasm32")]
mod graph;
#[cfg(target_arch = "wasm32")]
mod history;
#[cfg(target_arch = "wasm32")]
mod layout;
#[cfg(target_arch = "wasm32")]
mod render;
#[cfg(target_arch = "wasm32")]
mod rust_layout;
#[cfg(target_arch = "wasm32")]
mod selection;
#[cfg(target_arch = "wasm32")]
mod session;
#[cfg(target_arch = "wasm32")]
mod ui;
#[cfg(target_arch = "wasm32")]
mod visuals;
#[cfg(target_arch = "wasm32")]
mod web_app;

#[cfg(target_arch = "wasm32")]
pub use wasm_bindgen_rayon::init_thread_pool;
#[cfg(target_arch = "wasm32")]
pub use web_app::WebHandle;
