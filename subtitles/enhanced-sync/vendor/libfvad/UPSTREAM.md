# libfvad Source Snapshot

Vendored from libfvad upstream:

- repository: `https://github.com/dpirch/libfvad.git`
- commit: `532ab666c20d3cfda38bca63abbb0f152706c369`
- source date: `2024-02-19`
- vendored for: WebRTC voice activity detection in the enhanced subtitle sync
  plugin

The plugin build compiles this vendored tree as a narrow static C library and
links it into the final Rust `wasm32-wasip1` plugin artifact. libfvad is a
standalone extraction of the WebRTC VAD engine; it is licensed under
BSD-3-Clause, with the additional patent grant included in `PATENTS`.

Keep the vendored archive narrow: only `include`, `src`, and attribution files
needed to rebuild and document the VAD backend belong in the archive.
