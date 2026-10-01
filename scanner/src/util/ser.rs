//! Serde helpers.

use serde::{Serialize, Serializer};

/// An array of any length as a sequence (serde's own implementations stop at 32).
pub fn array<S: Serializer, T: Serialize, const N: usize>(v: &[T; N], s: S) -> Result<S::Ok, S::Error> {
    v.as_slice().serialize(s)
}
