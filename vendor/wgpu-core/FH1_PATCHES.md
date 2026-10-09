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
