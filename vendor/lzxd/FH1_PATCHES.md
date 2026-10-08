# Local patches to lzxd 0.2.7

Vendored from crates.io (MIT OR Apache-2.0) for decoding Forza Horizon's XMemCompress
(zip method 21) streams.

1. **No odd-length padding after uncompressed blocks.** Upstream skips one byte after an
   uncompressed block of odd size (the CAB rule). XMemCompress never writes that byte: the
   next block header follows the raw data directly. Proven on `UI.zip`, `Driver.zip` and
   `animatedobjects.zip`, where every entry now matches its zip CRC32. Before the patch, 16
   entries failed with `InvalidBlock` / `InvalidPathLengths` / `InvalidPretreeRle`.
2. **Raw runs are clamped to the frame's output length**, not only to the bytes left in
   the chunk, so a raw run never overshoots `output_len` when another block follows it in
   the same frame.
