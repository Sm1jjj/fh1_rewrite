// W4 merged-mesh LOD (batch.rs): per-placement LOD range + cross-fade inside one merged mesh.
// Vertex attribute ATTRIBUTE_INSTANCE_LOD = (centre.xyz, bitcast(pack2x16float(start, end))); shader def INSTANCE_LOD.
//
// Vertex stage:   let f = batch_lod_fades(lod, view.world_position);
//                 if batch_lod_hidden(f) { out.position = batch_lod_collapsed(); }   // whole placement drops out
//                 out.lod_fades = f;   // pass to the fragment (constant per placement)
// Fragment stage: if batch_lod_discard(in.lod_fades, in.position.xy) { discard; }
//
// The fades match the faithful path's VisibilityRange dither: a LOD fading out at distance d and the next one fading
// in at d use complementary halves of one screen-space pattern, so each pixel is drawn exactly once.
#define_import_path fh1_remaster::batch_lod

// Half-width of the fade band around a LOD boundary at distance d (m).
fn batch_lod_margin(d: f32) -> f32 {
    return clamp(d * 0.1, 2.0, 20.0);
}

// (fade-in, fade-out), each 0..1: drawn fully when fade-in = 1 and fade-out = 1.
fn batch_lod_fades(lod: vec4<f32>, camera: vec3<f32>) -> vec2<f32> {
    let range = unpack2x16float(bitcast<u32>(lod.w));
    let dist = distance(lod.xyz, camera);
    var fade_in = 1.0;
    if range.x > 0.0 {
        let m = batch_lod_margin(range.x);
        fade_in = smoothstep(range.x - m, range.x + m, dist);
    }
    var fade_out = 1.0;
    // unpack2x16float of 0x7C00 = +inf: never culled.
    if range.y < 65504.0 {
        let m = batch_lod_margin(range.y);
        fade_out = 1.0 - smoothstep(range.y - m, range.y + m, dist);
    }
    return vec2<f32>(fade_in, fade_out);
}

fn batch_lod_hidden(f: vec2<f32>) -> bool {
    return f.x <= 0.0 || f.y <= 0.0;
}

// Clip position outside the clip volume: every vertex of the placement lands here, so its triangles are degenerate.
fn batch_lod_collapsed() -> vec4<f32> {
    return vec4<f32>(2.0, 2.0, 2.0, 1.0);
}

// 4x4 Bayer threshold in [0, 1).
fn batch_lod_noise(frag: vec2<f32>) -> f32 {
    let p = vec2<u32>(frag) & vec2<u32>(3u);
    let bayer = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);
    return (bayer[p.y * 4u + p.x] + 0.5) / 16.0;
}

fn batch_lod_discard(f: vec2<f32>, frag: vec2<f32>) -> bool {
    if f.x >= 1.0 && f.y >= 1.0 {
        return false;
    }
    let n = batch_lod_noise(frag);
    // Fade-in keeps the upper part of the pattern, fade-out the lower: complementary for a LOD pair.
    return n < 1.0 - f.x || n >= f.y;
}
