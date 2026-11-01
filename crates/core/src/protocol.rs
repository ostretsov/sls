//! JSON documents exchanged between instances.
//!
//! * `GET /api/info` → [`Info`]
//! * `GET /api/list/<share>/<path…>` → `Vec<`[`Entry`]`>`
//! * `GET /f/<share>/<path…>` → the file itself (supports `Range`)
//! * `POST /api/offer` with an [`Offer`]: "please download this from me";
//!   the receiver answers later with `POST /api/offer-reply` and an
//!   [`OfferReply`] to the sender's `port`
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

/// A file or folder in one of the sender's shares, offered to one peer.
/// The receiver downloads it from the address the offer came from.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Offer {
    /// Random; the reply names it.
    pub id: String,
    pub from_id: String,
    pub from_name: String,
    /// The sender's HTTP port.
    pub port: u16,
    pub share: String,
    /// Segments inside `share`; the last one is the name.
    pub path: Vec<String>,
    pub dir: bool,
    pub size: u64,
}

impl Offer {
    pub fn name(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or(&self.share)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct OfferReply {
    pub id: String,
    pub accepted: bool,
}
