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

2. **Missing cascade entry = no shadow views for that view** (orchestrator 91, 2026-10-08, crash on map load).
   `prepare_lights` unwrapped `light.cascades.get(&view)` / `light.frusta.get(&view)` for every Camera3d view that sees
   the light's layers. fh1-remaster light.rs prunes the cascade entries of views it doesn't need, and its first rule
   (by render layers) raced with the car probe face switching layers in the same frame. Now a view without an entry
   despawns its directional shadow views and is skipped. light.rs since prunes only non-Camera3d views (never looked up
   here), so this is a safety net.

3. **Serial specialization gather** (dc, 2026-10-08, main-thread job). `check_entities_needing_specialization<M>`
   (`src/material.rs`) gathered changed entities with `par_iter` for every material type every frame: ~0.13-0.18 ms
   each x 10 material types in the user's log 20261008_154403, mostly task-pool scope cost for a handful of tick checks.
   Now a serial walk when the material has <= 4096 entities (`Query<(), With<MeshMaterial3d<M>>>::iter().len()`, cheap).
   `FH1_SPEC_PAR=1` = always parallel (upstream).
