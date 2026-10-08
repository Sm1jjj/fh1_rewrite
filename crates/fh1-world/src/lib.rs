//! Forza Horizon open world (Colorado): collision, surfaces and spatial queries.
//!
//! - [`col`]: parsers for the stream-square grid (`Ribbon_00/*_track_00.col`) and the per-square
//!   collision meshes (`<n>.fiz` in the track's `bin.zip`).
//! - [`surface`]: `media/physics.zip/surfaceTypes.xml`, which names the `surface_id` on every
//!   collision triangle and gives its friction, off-road factor, bumpiness and debug colour.
//! - [`world`]: the whole track welded into one mesh with a uniform XZ grid for ray and sphere
//!   queries, plus a compact file format so the game doesn't re-read the disc.
//!
//! Coordinates are the game's world space, unchanged: metres, +Y up. Whether the engine
//! needs to mirror Z (left- vs right-handed) is decided by the engine; see
//! `docs/COLORADO_RECON.md`.

pub mod col;
pub mod surface;
pub mod world;

pub use surface::Surface;
pub use world::{Contact, Hit, Tri, World};
