//! The `wren` command's components as a library: the server, the cache and
//! node tiers, the client and checkout, and the MCP server. The binary
//! (`main.rs`) is a thin command line over them; tests (notably the
//! deterministic simulation) run them in one process.

pub mod cache;
pub mod checkout;
pub mod client;
pub mod mcp;
pub mod server;
pub mod transport;
#[cfg(all(feature = "vfs", any(target_os = "linux", target_os = "macos")))]
pub mod vfs;
