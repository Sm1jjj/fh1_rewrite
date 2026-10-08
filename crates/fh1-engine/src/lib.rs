//! fh1-engine library: car data and the vehicle simulation, shared by the game and the parity tool.

pub mod data;
/// FH1's Performance Index / class / ratings calculator for upgraded cars (docs/PI.md).
pub mod pi;
/// FH1's stats harness on our sim (parity bin, upgrade PI; docs/PI.md).
pub mod stats;
pub mod ai;
pub mod traffic;
pub mod vehicle;
pub mod world;
