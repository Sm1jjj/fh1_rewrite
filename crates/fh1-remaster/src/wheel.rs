//! Remaster wheels (W2): rims, tyres, brake rotors, drums, hubs and calipers as PBR surfaces.
//!
//! The wheels stay the car glTF's own `wheel_XX` / `rotor_XX` / `caliper_XX` nodes (the vehicle code spins and steers
//! them); only their materials change, by Turn 10 material name (the same names on FH1, FH2 and FM4 cars, docs/WHEELS.md
//! "Car materials").

use crate::car::Look;

/// The look of a wheel or brake material, or None when `name` (lower case, `_noocclude`-style suffixes removed) is not one.
pub fn look(name: &str) -> Option<Look> {
    let n = name;
    // Tyres: tread/sidewall share one atlas (tire.png); dull rubber with a faint sheen on the sidewall lettering.
    if n == "tire" || n.starts_with("tire_") || n.starts_with("tread") || n.starts_with("sidewall") || n.contains("tyre") || n == "rubber" {
        return Some(Look { roughness: 0.82, reflectance: 0.3, ..Look::textured() });
    }
    // Chrome rims and lips: a mirror finish.
    if n.starts_with("chrome_rim") || n.starts_with("chrome_blur") {
        return Some(Look::chrome(0.06));
    }
    // Painted/alloy rims (wheel.png): the atlas carries the rim's own colour (white, silver, black, gold) under a
    // coat. Mostly dielectric: at metallic 0.85 the Corrado's white rims went dark grey (they mirrored the dark ground).
    if n == "rim" || n == "inner_rim" || n == "outer_rim" || n.starts_with("rim_") || n.starts_with("blur_rim") || n.starts_with("blur_lip") {
        return Some(Look { metallic: 0.25, roughness: 0.35, clearcoat: 0.8, clearcoat_roughness: 0.08, ..Look::textured() });
    }
    if n == "wheel_emblem" {
        return Some(Look { metallic: 0.6, roughness: 0.25, clearcoat: 1.0, clearcoat_roughness: 0.05, ..Look::textured() });
    }
    if n == "wheel_black" {
        return Some(Look { roughness: 0.55, reflectance: 0.4, ..Look::keep_colour() });
    }
    // Brakes: cast-iron discs and drums, painted calipers (the exterior atlas holds their colour and lettering).
    if n.starts_with("rotor") || n.starts_with("drum") {
        return Some(Look { metallic: 0.9, roughness: 0.42, ..Look::textured() });
    }
    if n.starts_with("hub") {
        return Some(Look { metallic: 0.7, roughness: 0.5, ..Look::textured() });
    }
    if n.starts_with("caliper") || n == "brake" || n == "white_brake" || n == "brake_badge" {
        return Some(Look { roughness: 0.35, clearcoat: 0.8, clearcoat_roughness: 0.1, ..Look::textured() });
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn wheel_names() {
        assert!(super::look("rim").unwrap().clearcoat > 0.5);
        assert!(super::look("tire").unwrap().metallic == 0.0);
        assert!(super::look("caliper").is_some());
        assert!(super::look("body").is_none());
        assert!(super::look("rubber_trim").is_none());
    }
}
