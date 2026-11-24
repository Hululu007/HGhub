//! HGhub Plugin Protocol host. No content providers or upstream-specific code.
mod content;
pub mod capabilities;
pub mod error;
mod forms;
pub mod ipc;
pub mod library;
pub mod manager;
pub mod media;
pub mod model;
mod native;
mod network;
pub mod proxy;
pub mod network_policy;
pub mod node;
pub mod python;
mod resolver_preferences;
mod runtime;
mod store;
pub mod web_login;
pub mod webhome;
pub mod webhome_data;
pub mod wire;

pub use error::{PluginError, Result};
pub use manager::PluginManager;
pub use network::{looks_like_challenge, ClearanceCookie};
