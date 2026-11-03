//! reaper's core: session lifecycle, site configuration, and the provider and
//! forge seams.
//!
//! This crate knows nothing about any hypervisor, any operating system, any
//! forge, or any tenant. That is not an aspiration -- lint guards in `tools/guards.sh` fail
//! the build when hypervisor vocabulary, OS-specific identifiers or tenant
//! names appear here, and the forge seam is guarded the same way.

pub mod config;
pub mod duration;
pub mod forge;
pub mod job;
pub mod paths;
pub mod provider;
pub mod session;
pub mod sync;
pub mod transport;

pub use config::{Config, ConfigError};
pub use forge::{Forge, ForgeError, Request};
pub use provider::{
    CreateRequest, Finding, Health, MachineRef, MachineSummary, Provider, ProviderError,
    RegisteredGuest,
};
pub use session::{Session, Store};
pub use transport::{Ssh, Transport};

#[cfg(test)]
mod tests;
