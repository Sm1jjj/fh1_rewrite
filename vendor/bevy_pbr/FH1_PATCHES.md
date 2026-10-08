# Local patches to bevy_pbr 0.19.1

Vendored from crates.io (MIT OR Apache-2.0), wired in by `[patch.crates-io]` in the workspace `Cargo.toml`. All
changes are in `src/render/light.rs` and marked `FH1 patch`. Diff against the registry copy to review them.
Rebase them when Bevy is upgraded (or drop them if upstream gains shadow caching).

1. **Directional shadow cascade caching** (docs/PERF.md "P8-B"; user of it: fh1-remaster `light.rs`, `FH1_RM_CASCADE_CACHE`).
   - New main-world resource `DirectionalShadowCache { enabled, skip_mask }`. The app adds
     `ExtractResourcePlugin::<DirectionalShadowCache>` and sets it each frame.
   - `prepare_lights`: with `enabled`, the directional shadow array texture is created once and kept in a `Local`
     (re-created only when its descriptor changes: map size or layer count), instead of being taken from the
     per-frame `TextureCache`, which can hand out a different texture every frame. It inserts
     `DirectionalShadowSkipThisFrame(mask)` (0 when caching is off or the texture was just created).
   - `per_view_shadow_pass`: a directional shadow view whose `cascade_index` bit is set in that mask is skipped: no
     render pass, so no depth clear and no draws; the layer keeps the depth from the frame that last drew it.
   - Contract: the app keeps the skipped cascade's `Cascade` (matrices, texel size) identical on skipped frames, so
     sampling matches what was drawn. Queue / specialize / batching still run for the skipped view (the encode and the
     GPU work are what is saved).
   - `enabled: false` (or no resource) = upstream behaviour.
