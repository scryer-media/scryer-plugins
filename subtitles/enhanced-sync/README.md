# Enhanced Subtitle Sync

Experimental subtitle sync decoder plugin.

This crate is intentionally not an official catalog plugin yet. The current
public Scryer plugin SDK has subtitle catalog and generator providers, but it
does not yet expose a subtitle-sync decoder provider contract. This crate starts
the Wasm-side implementation and keeps the exported contract small while the app
host interface is designed.

Initial scope:

- accept codec probe and decode-window requests for AC-3, E-AC-3, DTS, and
  TrueHD/MLP
- decode AC-3, E-AC-3, DTS/DCA, and TrueHD/MLP windows to interleaved f32
  little-endian PCM through vendored FFmpeg
- transcode mounted media inputs with targeted AC-3, E-AC-3, DTS/DCA,
  DTS-HD MA core fallback, and TrueHD/MLP streams to mono 16 kHz FLAC for
  Scryer subtitle sync and future AI generator reuse
- use lightweight frame sniffing to route packets before decode
- carry a pinned FFmpeg source snapshot for the compiled decoder backend
- keep Symphonia as the expected container demux and stream-selection layer in
  the Scryer app

Current decoder status:

- AC-3 / E-AC-3, DTS/DCA, and TrueHD/MLP are routed through a narrow vendored
  FFmpeg `avformat`/`avcodec`/`swresample`/`avutil` build linked into the
  plugin Wasm.
- Symphonia remains test-only here and represents the host-side packet shape
  Scryer will pass across the ABI.

The exported response uses base64-encoded interleaved `f32le` PCM so the host
can feed the same VAD/alignment path used by the in-process subtitle sync code.

The audio transcode export writes FLAC artifacts to a mounted writable output
directory. Re-vendor FFmpeg with:

```sh
cargo xtask ffmpeg revendor --commit <ffmpeg-commit>
```

Use `--source /path/to/FFmpeg` when refreshing from a local checkout. That
refresh rewrites both the human-readable `vendor/ffmpeg/UPSTREAM.md` and the
machine-readable `vendor/ffmpeg/SCRYER_VENDOR_METADATA` file that `build.rs`
uses to pin the upstream revision and invalidate stale FFmpeg build artifacts.
