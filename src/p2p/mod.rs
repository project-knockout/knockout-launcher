//! Player transport used by the launcher.
mod protocol;
pub use protocol::*;
#[path = "bridge_runtime.rs"]
pub mod bridge;
mod wire;
