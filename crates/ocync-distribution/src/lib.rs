//! OCI distribution client library — types, authentication, and registry operations.

/// Authentication providers and token management.
pub mod auth;
/// Blob operations (upload, download, existence checks).
pub mod blob;
/// OCI registry HTTP client.
pub mod client;
pub mod digest;
pub mod error;
pub mod manifest;
pub mod reference;
pub mod sha256;
pub mod spec;
pub mod tags;

pub use client::RegistryClient;
pub use digest::Digest;
pub use error::Error;
pub use reference::Reference;
pub use spec::{Descriptor, ImageIndex, ImageManifest, ManifestKind, Platform};
