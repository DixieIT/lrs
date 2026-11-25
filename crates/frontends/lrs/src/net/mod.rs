//! Legacy HTTP bridge to `serve`. Compiled only with `legacy-http`.
//! The wire types live in `crate::protocol` and are shared by both backends.

#[cfg(feature = "legacy-http")]
pub mod client;
#[cfg(feature = "legacy-http")]
pub mod spawn;

#[cfg(feature = "legacy-http")]
pub use client::Client;
#[cfg(feature = "legacy-http")]
pub use spawn::ensure_server;
