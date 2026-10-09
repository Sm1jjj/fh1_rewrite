# Local patches to wgpu-core 29.0.4

Vendored from crates.io (MIT OR Apache-2.0), wired in by `[patch.crates-io]` in the workspace `Cargo.toml`. All
changes are in `src/command/{pass,render,compute}.rs` and marked `FH1 patch`. Diff against the registry copy to review
them. Rebase them when wgpu is upgraded (or drop them if upstream stops re-walking re-bound groups).

1. **Bind groups merged / init-checked once per render pass** (P16, 2026-10-09; docs/PERF.md "P16").
   `pass::set_bind_group` merged the group's every buffer and texture view into the pass's usage scope on EVERY
   `set_bind_group`, and `flush_bindings_helper` re-registered every texture's memory-init action on every re-bind.
   A bindless RemasterMaterial slab holds hundreds of texture views, and the static world re-binds each slab once
   per draw class per pass (and Bevy's transparent pass re-binds it between interleaved draws), so this was most of
   the render thread's `draw_static_shadows` (2.1 ms/frame) / `draw_static_world` (1.0) / transparent (0.9) time in
   user log 20261008_225502.
   - `PassState.merged_bind_groups`: tracker indices already merged into `scope`; a repeat merge is skipped (same
     resources with the same usages cannot change the scope or create a conflict).
   - `PassState.init_bind_groups` (render passes only; `None` for compute): groups whose memory-init actions were
     already registered in this pass; a re-bind skips the walk (the first registration already initialises the memory
     before the pass; nothing inside a render pass discards a sampled resource).

2. **Usage scopes reset sparsely** (P16-A, worker site-75, 2026-10-09; docs/PERF_P16_A.md). `UsageScope::drop` called
   `clear()` (empties the state vectors) and the next `new_pooled` called `set_size(device size)`, refilling every slot
   of every buffer / texture alive on the device, for each of the ~60-90 render / compute passes per frame. Now
   `clear_sparse()` resets only the owned slots to the same defaults (`BufferUses::empty()`, `TextureUses::UNINITIALIZED`)
   and keeps the lengths, so `set_size` is a no-op unless the device grew. The compute path's mid-pass removals
   (`set_and_remove_from_usage_scope_sparse`) now also reset the removed slot, so every slot reads exactly as after the
   old clear + resize. Revert = the two `clear()` calls in `track/mod.rs`.

3. **Bind groups tracked once per command buffer** (P18, 2026-10-09; docs/PERF.md "P18"). `command/pass.rs` `set_bind_group`
   pushed the bind group into the command buffer's `trackers.bind_groups` (`StatelessTracker`, a plain `Vec<Arc<_>>`) on
   EVERY call, duplicates included, and `device/queue.rs` `validate_command_buffer` calls `BindGroup::try_raw` on every
   entry at submit, which walks all of the group's buffers and textures. A bindless RemasterMaterial slab holds hundreds
   of texture views and is re-bound per draw bin (static world lit classes, cutout cascade bins, Bevy's passes), so
   submit re-walked each slab's textures once per re-bind (part of `submit_pending_command_buffers` 1.39 ms, user log
   20261009_143733), and the duplicate Arcs were dropped again when the submission retired.
   `track/stateless.rs`: `StatelessTracker::insert_single_once(resource, key)` keeps a key -> position map and pushes a
   resource only once per tracker; pass.rs uses it with the bind group's tracker index (unique while the tracker holds
   the group alive). The tracker only keeps resources alive and lets submit validate them, so one entry per bind group
   is equivalent. Render bundles (`command/bundle.rs`) keep `insert_single`. No flag; revert = `insert_single` in pass.rs.
