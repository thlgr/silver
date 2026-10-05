//! Content hashing. Approval bookkeeping keys a remembered decision on the hash of a tool
//! call's canonical arguments.

use sha2::{Digest, Sha256};

/// Hex SHA-256 of `text`.
pub fn content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}
