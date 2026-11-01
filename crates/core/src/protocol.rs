//! JSON documents exchanged between instances.
//!
//! * `GET /api/info` → [`Info`]
//! * `GET /api/list/<share>/<path…>` → `Vec<`[`Entry`]`>`
//! * `GET /f/<share>/<path…>` → the file itself (supports `Range`)
//!
//! Every path segment is percent-encoded.

use serde::{Deserialize, Serialize};

/// Bumped on incompatible changes; peers with another value are still shown.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Info {
    pub id: String,
    pub name: String,
    pub version: String,
    pub protocol: u32,
    pub shares: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    /// Bytes (0 for directories).
    pub size: u64,
    /// Modification time, seconds since the Unix epoch.
    pub modified: i64,
}
