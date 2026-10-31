pub mod crypto;
pub mod game;
pub mod session;

#[cfg(not(target_arch = "wasm32"))]
pub mod net;
#[cfg(target_arch = "wasm32")]
pub mod net {
    pub mod message;
}

#[cfg(not(target_arch = "wasm32"))]
pub mod sim;
#[cfg(not(target_arch = "wasm32"))]
pub mod ui;
#[cfg(target_arch = "wasm32")]
mod wasm;
