//! Cracked Anark property names (hash → name), from `ahash` over candidate names checked against
//! value ranges in every scene (see `docs/UI.md`). Keys are compared on their 27-bit hash, so the
//! type bits (and the dynamic flag) don't matter.

use crate::hash::{ahash27, ahash31, MASK27};

macro_rules! props {
    ($($c:ident = $s:literal),* $(,)?) => {
        $(pub const $c: u32 = ahash27($s.as_bytes());)*
        /// Every known property: (27-bit hash, name).
        pub const PROPERTIES: &[(u32, &str)] = &[$(($c, $s)),*];
    };
}

props! {
    ENDTIME = "endtime", TIMEOFFSET = "timeoffset",
    POSITION_X = "position.x", POSITION_Y = "position.y", POSITION_Z = "position.z",
    ROTATION_X = "rotation.x", ROTATION_Y = "rotation.y", ROTATION_Z = "rotation.z",
    SCALE_X = "scale.x", SCALE_Y = "scale.y", SCALE_Z = "scale.z",
    PIVOT_X = "pivot.x", PIVOT_Y = "pivot.y", PIVOT_Z = "pivot.z",
    OPACITY = "opacity", ROTATIONORDER = "rotationorder", ORIENTATION = "orientation", PRIORITY = "priority",
    DIFFUSE_R = "diffuse.r", DIFFUSE_G = "diffuse.g", DIFFUSE_B = "diffuse.b", DIFFUSE_A = "diffuse.a",
    AMBIENT_R = "ambient.r", AMBIENT_G = "ambient.g", AMBIENT_B = "ambient.b", AMBIENT_A = "ambient.a",
    EMISSIVEPOWER = "emissivepower", BLENDMODE = "blendmode",
    POSITIONU = "positionu", POSITIONV = "positionv", ROTATIONUV = "rotationuv",
    SCALEU = "scaleu", SCALEV = "scalev", PIVOTU = "pivotu", PIVOTV = "pivotv",
    TILINGMODEHORZ = "tilingmodehorz", TILINGMODEVERT = "tilingmodevert",
    TEXTSTRING = "textstring", FONT = "font", SIZE = "size", LEADING = "leading", TRACKING = "tracking",
    HORZALIGN = "horzalign", VERTALIGN = "vertalign", RENDERSTYLE = "renderstyle", TEXTTYPE = "texttype",
    WORDWRAP = "wordwrap", USEBACKCOLOR = "usebackcolor", BOXWIDTH = "boxwidth", BOXHEIGHT = "boxheight",
    TEXTCOLOR_R = "textcolor.r", TEXTCOLOR_G = "textcolor.g", TEXTCOLOR_B = "textcolor.b", TEXTCOLOR_A = "textcolor.a",
    BACKCOLOR_R = "backcolor.r", BACKCOLOR_G = "backcolor.g", BACKCOLOR_B = "backcolor.b", BACKCOLOR_A = "backcolor.a",
    FOV = "fov", CLIPNEAR = "clipnear", CLIPFAR = "clipfar", ORTHOGRAPHIC = "orthographic",
    LOOKATLOCK = "lookatlock", FOGENABLE = "fogenable", FOGMODE = "fogmode", FOGTYPE = "fogtype",
    FOGNEAR = "fognear", FOGFAR = "fogfar",
    FOGCOLOR_R = "fogcolor.r", FOGCOLOR_G = "fogcolor.g", FOGCOLOR_B = "fogcolor.b", FOGCOLOR_A = "fogcolor.a",
    LIGHTAMBIENT_R = "lightambient.r", LIGHTAMBIENT_G = "lightambient.g", LIGHTAMBIENT_B = "lightambient.b",
    LIGHTSPECULAR_R = "lightspecular.r", LIGHTSPECULAR_G = "lightspecular.g", LIGHTSPECULAR_B = "lightspecular.b",
    LIGHTDIFFUSE_R = "lightdiffuse.r", LIGHTDIFFUSE_G = "lightdiffuse.g", LIGHTDIFFUSE_B = "lightdiffuse.b",
}

/// Behaviour prop (string) whose value is `"contract"` on data-binding behaviours. Name not cracked.
pub const CONTRACT_KIND: u32 = 0x064C_B58A;
/// Behaviour prop (string) holding the contract type (`UIContract`, `PAUSE_MENU_BUTTON`, …). Name not cracked.
pub const CONTRACT_TYPE: u32 = 0x04A9_EC63;

/// 31-bit event hashes of a few common game events (full list: `UI.zip/Scenes/ui4/EventNames.txt`).
pub const EVENT_SHOWN: u32 = ahash31(b"SHOWN");
pub const EVENT_HIDDEN: u32 = ahash31(b"HIDDEN");
pub const EVENT_FOCUSED: u32 = ahash31(b"FOCUSED");
pub const EVENT_INTRO: u32 = ahash31(b"INTRO");
pub const EVENT_OUTRO: u32 = ahash31(b"OUTRO");

/// Name of a property key (type bits ignored).
pub fn prop_name(key: u32) -> Option<&'static str> {
    let h = key & MASK27;
    PROPERTIES.iter().find(|(k, _)| *k == h).map(|(_, n)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(OPACITY, 0x0191_C315);
        assert_eq!(prop_name((3 << 27) | POSITION_X), Some("position.x"));
        assert_eq!(prop_name((21 << 27) | TEXTSTRING), Some("textstring"));
        assert_eq!(prop_name(0x0793_DEB3), None);
        assert_eq!(EVENT_SHOWN, 0x519E_02AF);
        // no two names collide on 27 bits
        for (i, a) in PROPERTIES.iter().enumerate() {
            assert!(PROPERTIES[i + 1..].iter().all(|b| b.0 != a.0), "{}", a.1);
        }
    }
}
