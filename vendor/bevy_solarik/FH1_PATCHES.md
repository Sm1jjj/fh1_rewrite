# FH1 patches to bevy_solarik v0.1.0

Vendored from github.com/AlrikOlson/bevy_solarik v0.1.0 (MIT/Apache) for the remaster RTX mode (fh1-remaster `rtx` feature).

1. `src/scene/blas.rs` `prepare_raytracing_blas`: when a BLAS is (re)built, drop any older compaction-queue entry for
   the same mesh. Without it a mesh whose BLAS was rebuilt (opacity change: the same mesh first seen with an opaque,
   then an alpha-masked material, or a modified mesh) was queued twice and the new BLAS got `prepare_compaction_async`
   twice: wgpu validation error "Compaction is already being prepared", which quits the app.
