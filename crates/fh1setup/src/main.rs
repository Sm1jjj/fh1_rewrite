//! fh1setup: turn the user's own Forza Horizon disc into assets for fh1-engine.
//!
//! ```text
//! fh1setup <disc.iso | extracted disc folder | default.xex> [--data <dir>] [--force] [--only <group>,...]
//! ```
//!
//! Layout (modelled on skate3rust):
//! ```text
//! <data>/installation.json                       which install is active + per-group pipeline hashes
//! <data>/installations/<id>/assets/private/<group>/...
//! <data>/work/                                   scratch (ISO extraction)
//! ```
//! Optional importers, built only with their cargo feature (off by default; `--features fh2,fm4`):
//! - `fh2`: Forza Horizon 2 (Xbox 360) cars + map into the active install (`imported/fh2`, docs/FH2_RECON.md):
//!   `fh1setup import-fh2 <FH2 disc.iso | extracted folder> [--data <dir>] [--only cars,map]`.
//! - `fm4`: Forza Motorsport 4 cars + circuits (`imported/fm4`, docs/FM4_RECON.md): `fh1setup import-fm4 ...`.
//!
//! `<id>` is derived from the SHA-256 of `default.xex`, so one edition maps to one install.
//! A group is rebuilt only when its pipeline hash changes (or with `--force`).
#![recursion_limit = "256"]

mod anim;
mod story;
mod missions;
mod airborne_objects;
mod audio;
mod camera;
mod ailines;
mod effects;
mod traffic;
mod upgrades;
mod variants;
mod events;
mod carfx;
mod cartex;
mod cockpit;
mod crowd;
mod cars;
mod dynamicpost;
mod extract;
mod fmv;
#[cfg(feature = "fh2")]
mod fh2;
#[cfg(feature = "fm4")]
mod fm4;
#[cfg(feature = "fm4")]
mod fm4_merge;
mod grass;
mod install;
mod model;
mod remaster;
mod scenery;
mod shaders;
mod sky;
mod tracks;
mod textures;
mod world;
mod xml;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

/// Asset groups and their pipeline versions. Bump a version when its output changes.
pub const GROUPS: &[(&str, &str)] = &[("cars", "cars-18"), ("world", "world-3"), ("scenery", "scenery-33"), ("shaders", "shaders-2"), ("tracks", "tracks-3"), ("dynamicpost", "dynamicpost-2"), ("audio", "audio-3"), ("radio", "radio-3"), ("ui", "ui-5"), ("grass", "grass-1"), ("crowd", "crowd-2"), ("anim", "anim-2"), ("sky", "sky-1"), ("camera", "camera-1"), ("effects", "effects-1"), ("events", "events-8"), ("ailines", "ailines-2"), ("traffic", "traffic-1"), ("upgrades", "upgrades-3"), ("variants", "variants-2"), ("remaster", "remaster-5"), ("fmv", "fmv-1"), ("story", "story-1"), ("missions", "missions-2")];

struct Args {
    source: PathBuf,
    data: PathBuf,
    force: bool,
    only: Option<Vec<String>>,
}

fn parse_args() -> Result<Args> {
    let mut source = None;
    let mut data = PathBuf::from("data");
    let mut force = false;
    let mut only = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data" => data = it.next().context("--data needs a path")?.into(),
            "--force" => force = true,
            "--only" => {
                only = Some(
                    it.next()
                        .context("--only needs group names")?
                        .split(',')
                        .map(str::to_owned)
                        .collect(),
                )
            }
            "-h" | "--help" => {
                println!(
                    "usage: fh1setup <disc.iso | disc folder | default.xex> [--data <dir>] [--force] [--only cars]"
                );
                std::process::exit(0);
            }
            _ if source.is_none() => source = Some(PathBuf::from(a)),
            _ => bail!("unexpected argument {a}"),
        }
    }
    Ok(Args {
        source: source.context("pass your Forza Horizon ISO or extracted disc folder")?,
        data,
        force,
        only,
    })
}

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        #[cfg(feature = "fh2")]
        Some("import-fh2") => return fh2::run(),
        #[cfg(feature = "fm4")]
        Some("import-fm4") => return fm4::run(),
        #[allow(unreachable_patterns)]
        Some(c @ ("import-fh2" | "import-fm4")) => bail!("{c}: rebuild fh1setup with `--features {}`", &c[7..]),
        _ => {}
    }
    let args = parse_args()?;
    std::fs::create_dir_all(&args.data)?;
    let disc = extract::resolve_disc(&args.source, &args.data.join("work"))?;
    println!("disc: {}", disc.display());
    install::run(&disc, &args.data, args.force, args.only.as_deref())
}
