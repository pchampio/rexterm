//! Rex's control and data protocols, independent of the GUI event loop.
pub mod snapshot;
pub mod transport;
pub mod wire;

pub mod actor;
mod restore;
pub use ghostty_vt;

pub mod graphics;

pub mod upload;
pub mod forward;

pub mod bridge;
