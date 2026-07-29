//! Lowercase hex encoding for digest output.
//!
//! `digest` 0.11 returns `hybrid_array::Array` instead of `GenericArray`, and
//! that type has no `LowerHex` impl, so the old `format!("{:x}", ..)` on a
//! digest no longer compiles. These hashes key persisted data — account history
//! rows, journal artifacts, lock filenames — so the encoding must stay exactly
//! what `{:x}` produced: lowercase, two digits per byte, no separators.

/// Encode bytes as lowercase hex, two digits per byte.
pub fn lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}
