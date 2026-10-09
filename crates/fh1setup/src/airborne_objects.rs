//! The animated aircraft of the airborne showcase challenges (fh1-engine `race/airborne.rs`, scripted_content.md 3.8):
//! `ANIM_Showcase_Event_*` and `ANIM_Timed_Event_Test_Animation`. They are not in Colorado's `bin.zip` (the `anim` group
//! only takes the 33 type-4 objects placed there) but in `media/animatedobjects.zip` (290 objects), same type-4 `.pgeo`
//! format (`fh1_formats::granny::parse_anim_object` parses all eight, VERIFIED: 120..200 s clips, one LOD, no PVS
//! texture references).
//!
//! `append` is called by `anim::build` after it has built the object list: the files are written as
//! `objects/<next index>.pgeo` and listed at the end of `index.json`'s `objects` (`{name, file, textures: {}}`), so the
//! engine's `AnimWorld` finds them by name; no scene instance refers to them (the engine adds one driven instance, see
//! race/airborne.rs). Textures: the objects name their `.xds` textures (`ANIM_P-51_Mustang_DIFF`, ...) as strings in the
//! file instead of PVS indices; binding them is not done (UNKNOWN slot mapping), so the aircraft draw untextured.

use std::path::Path;

use anyhow::Result;
use fh1_formats::{granny, zip::Archive};
use serde_json::json;

/// The in-race objects of the seven events (airborne_challenges.xml `anim_in_race`).
pub const OBJECTS: [&str; 7] = [
    "ANIM_Timed_Event_Test_Animation",
    "ANIM_Showcase_Event_Helicopter_Race",
    "ANIM_Showcase_Event_Helicopter_Race_2",
    "ANIM_Showcase_Event_Biplane_Race",
    "ANIM_Showcase_Event_Biplane_Race_2",
    "ANIM_Showcase_Event_P51_Mustang_2",
    "ANIM_Showcase_Event_Balloon_Race",
];

/// Writes the aircraft into `<dir>/objects/` and appends them to `objects_json`; returns how many were added.
pub fn append(disc: &Path, dir: &Path, objects_json: &mut Vec<serde_json::Value>) -> Result<usize> {
    let zip = disc.join("media/animatedobjects.zip");
    if !zip.is_file() {
        println!("[anim] {} not found: the airborne aircraft are skipped", zip.display());
        return Ok(0);
    }
    let mut ar = Archive::open(&zip)?;
    let mut added = 0;
    for want in OBJECTS {
        let file = format!("{}.pgeo", want.to_ascii_lowercase());
        let found = ar.entries.iter().find(|e| {
            let n = e.name.replace('\\', "/").to_ascii_lowercase();
            n == file || n.ends_with(&format!("/{file}"))
        });
        let Some(e) = found.cloned() else {
            println!("[anim] {want}: not in animatedobjects.zip");
            continue;
        };
        let d = ar.read(&e)?;
        if let Err(err) = granny::parse_anim_object(&d) {
            println!("[anim] {want}: not a readable type-4 object ({err}); skipped");
            continue;
        }
        let i = objects_json.len();
        std::fs::write(dir.join(format!("objects/{i}.pgeo")), &d)?;
        objects_json.push(json!({ "name": want, "file": format!("objects/{i}.pgeo"), "textures": {} }));
        added += 1;
    }
    println!("[anim] {added} airborne aircraft objects added from animatedobjects.zip");
    Ok(added)
}
