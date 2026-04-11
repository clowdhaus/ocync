//! OCI distribution client library — types, authentication, and registry operations.

pub mod digest;
pub mod error;
pub mod reference;
pub mod sha256;
pub mod spec;

pub use digest::Digest;
pub use error::Error;
pub use reference::Reference;
pub use spec::{Descriptor, ImageIndex, ImageManifest, ManifestKind, Platform};
