//! Local HTTP control API: bearer auth, read endpoints, and server lifecycle.

pub mod handlers;
pub mod server;
pub mod types;
pub mod ws;

pub use server::IntegrationsManager;
