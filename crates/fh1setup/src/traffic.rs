//! `traffic` group: the free-roam traffic data for the engine's traffic system (fh1-engine traffic/, docs/TRAFFIC.md).
//!
//! Output:
//! - `AIOpenWorld.xml`: as stored in `media/gametunablesettings.zip` (car lists, car groups, densities per
//!   `traffic_density` id; parsed by `fh1_engine::traffic::config`).
//! - `colorado.nav`: the road network (`media/tracks/colorado/colorado.nav`; ways carry `road_type`, `oneway`,
//!   `traffic_density`, `traffic_disabled`; parsed by `fh1_ui::nav`).
//! - `cars.json`: `{ "<Data_Car Id>": "<MediaName>" }` for every car (the XML and FreeRoamDrivers name cars by Id).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use fh1_formats::zip::Archive;
use rusqlite::{Connection, OpenFlags};

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let mut ar = Archive::open(disc.join("media/gametunablesettings.zip")).context("media/gametunablesettings.zip")?;
    let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case("AIOpenWorld.xml")).cloned().context("AIOpenWorld.xml not in gametunablesettings.zip")?;
    std::fs::write(out.join("AIOpenWorld.xml"), ar.read(&e)?)?;
    std::fs::copy(disc.join("media/tracks/colorado/colorado.nav"), out.join("colorado.nav")).context("colorado.nav")?;
    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = db.prepare("SELECT Id, MediaName FROM Data_Car")?;
    let cars: BTreeMap<String, String> = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?.to_string(), r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<_>>()?;
    std::fs::write(out.join("cars.json"), serde_json::to_vec_pretty(&cars)?)?;
    println!("[traffic] AIOpenWorld.xml, colorado.nav, {} car ids", cars.len());
    Ok(())
}
