//! JSON decoding shared by the registry and AWS clients.

use serde::Deserialize;

/// Read a JSON `null` as the type's default, as the SDK reads a field that
/// is null or absent and as some registries send an empty list. Use with
/// `#[serde(default)]` so an absent field reads the same.
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(<Option<T>>::deserialize(deserializer)?.unwrap_or_default())
}
