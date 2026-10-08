//! Mip chains for imported textures (imported maps and cars).
//!
//! The importers write plain PNGs and Bevy's glTF loader uploads them with a single level, so distant
//! surfaces sampled the full-resolution image: shimmering, grainy roads and walls on every imported map.
//! FH1's own textures are DDS with the game's mips and never reach this path (compressed formats).
//! When an uncompressed RGBA8 2D image with one level is added, this builds a box-filtered chain (in linear
//! space for sRGB) and switches its sampler to trilinear + 16x anisotropic, keeping the address modes.
//! `FH1_MIPGEN=0` turns it off.

use bevy::image::{ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::prelude::*;
use bevy::render::render_resource::{TextureDimension, TextureFormat, TextureUsages};

pub struct MipGenPlugin;

impl Plugin for MipGenPlugin {
    fn build(&self, app: &mut App) {
        if std::env::var("FH1_MIPGEN").is_ok_and(|v| v == "0") {
            return;
        }
        app.init_resource::<MipJobs>().add_systems(PostUpdate, add_mips);
    }
}

/// Mip chains being built on AsyncCompute threads (2026-10-08 perf: built in add_mips on the main thread, 2048^2 car
/// atlases took 21-28 ms frames while cars and maps loaded). The image draws without mips for the few frames until its
/// chain lands. FH1_MIPGEN_ASYNC=0 = build in the system, as before.
#[derive(Resource, Default)]
struct MipJobs(Vec<(AssetId<Image>, usize, bevy::tasks::Task<(Vec<u8>, u32)>)>);

fn mipgen_async() -> bool {
    !std::env::var("FH1_MIPGEN_ASYNC").is_ok_and(|v| v == "0")
}

fn add_mips(mut events: MessageReader<AssetEvent<Image>>, mut images: ResMut<Assets<Image>>, mut jobs: ResMut<MipJobs>, fonts: Option<Res<bevy::text::FontAtlasSet>>) {
    // bevy_text font atlases are written glyph by glyph after they are added: a chain built from a snapshot wiped later
    // glyphs (2026-10-08: the race HUD's blank "2"). Never touch them.
    let font_atlas = |id: AssetId<Image>| fonts.as_ref().is_some_and(|f| f.values().flatten().any(|a| a.texture.id() == id));
    let added: Vec<AssetId<Image>> = events
        .read()
        .filter_map(|e| match e {
            AssetEvent::Added { id } => Some(*id),
            _ => None,
        })
        .filter(|id| !font_atlas(*id))
        .collect();
    for id in added {
        let Some(image) = images.get(id) else { continue };
        if !wants_mips(image) {
            continue;
        }
        if mipgen_async() {
            let d = &image.texture_descriptor;
            let (w, h, srgb) = (d.size.width as usize, d.size.height as usize, d.format == TextureFormat::Rgba8UnormSrgb);
            let base = image.data.clone().unwrap_or_default();
            let len = base.len();
            jobs.0.push((id, len, bevy::tasks::AsyncComputeTaskPool::get().spawn(async move { chain(base, w, h, srgb) })));
            continue;
        }
        let Some(mut image) = images.get_mut(id) else { continue };
        build_chain(&mut image);
    }
    // Finished chains: applied only if the image is still the one the job read (same single-level size).
    if jobs.0.is_empty() || !jobs.0.iter().any(|j| j.2.is_finished()) {
        return;
    }
    let mut left = Vec::new();
    for (id, len, task) in std::mem::take(&mut jobs.0) {
        if !task.is_finished() {
            left.push((id, len, task));
            continue;
        }
        let (data, levels) = bevy::tasks::block_on(task);
        if !font_atlas(id) && images.get(id).is_some_and(|i| wants_mips(i) && i.data.as_ref().is_some_and(|b| b.len() == len)) {
            if let Some(mut image) = images.get_mut(id) {
                image.data = Some(data);
                image.texture_descriptor.mip_level_count = levels;
                mip_sampler(&mut image);
            }
        }
    }
    jobs.0 = left;
}

fn wants_mips(image: &Image) -> bool {
    let d = &image.texture_descriptor;
    d.mip_level_count == 1
        && d.dimension == TextureDimension::D2
        && d.size.depth_or_array_layers == 1
        && d.size.width >= 8
        && d.size.height >= 8
        && matches!(d.format, TextureFormat::Rgba8UnormSrgb | TextureFormat::Rgba8Unorm)
        && !d.usage.contains(TextureUsages::RENDER_ATTACHMENT)
        && image.data.as_ref().is_some_and(|b| b.len() == (d.size.width * d.size.height * 4) as usize)
        // Never an all-zero image: bevy_text font atlases start zeroed and get glyphs written later; a mip chain built
        // from the empty snapshot wiped glyphs added meanwhile (2026-10-08: the race HUD's blank "2").
        && image.data.as_ref().is_some_and(|b| b.iter().any(|&x| x != 0))
}

fn build_chain(image: &mut Image) {
    let srgb = image.texture_descriptor.format == TextureFormat::Rgba8UnormSrgb;
    let (w, h) = (image.texture_descriptor.size.width as usize, image.texture_descriptor.size.height as usize);
    let Some(base) = image.data.take() else { return };
    let (out, levels) = chain(base, w, h, srgb);
    image.data = Some(out);
    image.texture_descriptor.mip_level_count = levels;
    mip_sampler(image);
}

/// The full chain (level 0 = `base`) and its level count; box filter in linear light.
fn chain(base: Vec<u8>, mut w: usize, mut h: usize, srgb: bool) -> (Vec<u8>, u32) {
    let to_lin: Vec<f32> = (0..256).map(|i| decode(i as f32 / 255.0, srgb)).collect();
    // Linear -> 8-bit through a table (4096 steps; a powf per channel per pixel was most of the cost).
    let to_u8: Vec<u8> = (0..=ENC_STEPS).map(|i| encode(i as f32 / ENC_STEPS as f32, srgb)).collect();
    let enc = |v: f32| to_u8[(v.clamp(0.0, 1.0) * ENC_STEPS as f32 + 0.5) as usize];
    let mut out = base.clone();
    let mut level: Vec<f32> = base.chunks_exact(4).flat_map(|p| [to_lin[p[0] as usize], to_lin[p[1] as usize], to_lin[p[2] as usize], p[3] as f32 / 255.0]).collect();
    let mut levels = 1;
    while w > 1 || h > 1 {
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let mut next = vec![0.0f32; nw * nh * 4];
        for y in 0..nh {
            let (y0, y1) = ((2 * y).min(h - 1), (2 * y + 1).min(h - 1));
            for x in 0..nw {
                let (x0, x1) = ((2 * x).min(w - 1), (2 * x + 1).min(w - 1));
                for c in 0..4 {
                    let s = level[(y0 * w + x0) * 4 + c] + level[(y0 * w + x1) * 4 + c] + level[(y1 * w + x0) * 4 + c] + level[(y1 * w + x1) * 4 + c];
                    next[(y * nw + x) * 4 + c] = s * 0.25;
                }
            }
        }
        out.extend(next.chunks_exact(4).flat_map(|p| [enc(p[0]), enc(p[1]), enc(p[2]), (p[3] * 255.0 + 0.5) as u8]));
        (level, w, h) = (next, nw, nh);
        levels += 1;
    }
    (out, levels)
}

const ENC_STEPS: usize = 4095;

fn mip_sampler(image: &mut Image) {
    let mut s = match &image.sampler {
        ImageSampler::Descriptor(d) => d.clone(),
        ImageSampler::Default => ImageSamplerDescriptor::default(),
    };
    s.mag_filter = ImageFilterMode::Linear;
    s.min_filter = ImageFilterMode::Linear;
    s.mipmap_filter = ImageFilterMode::Linear;
    s.anisotropy_clamp = 16;
    image.sampler = ImageSampler::Descriptor(s);
}

fn decode(v: f32, srgb: bool) -> f32 {
    if !srgb {
        v
    } else if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn encode(v: f32, srgb: bool) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let e = if !srgb {
        v
    } else if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (e * 255.0 + 0.5) as u8
}
