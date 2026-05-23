# FFmpeg Source Snapshot

Vendored from FFmpeg upstream:

- repository: `https://github.com/FFmpeg/FFmpeg`
- commit: `268c37fdae607be5d961708140f165cf26ca3483`
- source date: `2026-05-23`
- vendored for: AC-3, E-AC-3, DTS/DCA, and TrueHD/MLP decoder reference code

The files in this directory are not compiled by the initial plugin crate. They
are the pinned source baseline for the decoder port/Wasm backend. FFmpeg source
files are licensed by FFmpeg under LGPL-2.1-or-later unless the individual file
states otherwise. The copied decoder files in this snapshot carry LGPL headers.

Do not broaden this snapshot to all of FFmpeg without a concrete build need.
