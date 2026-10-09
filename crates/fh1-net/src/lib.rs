//! Free-roam multiplayer for the FH1 rewrite (docs/MULTIPLAYER.md).
//!
//! Each player simulates their own car. This crate moves snapshots between them: a [`Client`] for the game, a
//! [`Relay`] (one map, up to 24 players by default) for `fh1-server`, a [`Registry`] (the server list, `fh1-server
//! --registry`) that game servers [`Heartbeat`] to, and a [`ServerBrowser`] for the in-game list.
//! No physics, no assets, no Bevy, std only (builds on Windows and Linux).

mod browser;
mod client;
mod proto;
mod registry;
mod relay;

pub use client::{Client, Event, InfoQuery, LOST_AFTER};
pub use proto::{
    clean, decode, encode, password_proof, peek_version, resolve, sanitize_name, Packet, PlayerInfo, RejectReason, ServerInfo, Snapshot, WheelExt, FLAG_BRAKE,
    FLAG_CUSTOM_PAINT, FLAG_METALLIC, FLAG_REVERSE, KIT_SLOTS, KIT_STOCK, LOOK_CAGE, MAX_PLAYERS, NAME_MAX_CHARS, RIM_LEN, VERSION,
};
pub use browser::{BrowserRow, ServerBrowser};
pub use registry::{Heartbeat, Registry, ENTRY_MS, HEARTBEAT_MS};
pub use relay::{Config, Handled, Relay};
