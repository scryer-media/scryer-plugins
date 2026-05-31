# Enhanced Subtitle Sync

Beta subtitle sync decoder plugin.

This crate publishes as an official Scryer catalog plugin, but it remains a
beta add-on rather than a normal subtitle search provider. The current public
Scryer plugin SDK still models installation through the subtitle-provider
surface while Scryer consumes the decoder and alignment exports for enhanced
subtitle sync.

Initial scope:

- accept codec probe and decode-window requests for AC-3, E-AC-3, DTS, and
  TrueHD/MLP
- decode AC-3, E-AC-3, DTS/DCA, and TrueHD/MLP windows to interleaved f32
  little-endian PCM through vendored FFmpeg
- stream mounted media inputs with targeted AC-3, E-AC-3, DTS/DCA,
  DTS-HD MA core fallback, and TrueHD/MLP streams through vendored FFmpeg into
  mono 16 kHz PCM chunks for Scryer subtitle sync
- use lightweight frame sniffing to route packets before decode
- carry a pinned FFmpeg source snapshot for the compiled decoder backend
- carry a pinned libfvad source snapshot for WebRTC voice activity detection
- keep Symphonia as the expected container demux and stream-selection layer in
  the Scryer app

Current decoder status:

- AC-3 / E-AC-3, DTS/DCA, and TrueHD/MLP are routed through a narrow vendored
  FFmpeg `avformat`/`avcodec`/`swresample`/`avutil` build linked into the
  plugin Wasm.
- Symphonia remains test-only here and represents the host-side packet shape
  Scryer will pass across the ABI.
- WebRTC VAD is routed through a narrow vendored libfvad build linked into the
  plugin Wasm. Runtime labels use `webrtc-vad`; attribution names libfvad.

The exported response uses base64-encoded interleaved `f32le` PCM so the host
can feed the same VAD/alignment path used by the in-process subtitle sync code.

## Attribution

The Rust subtitle alignment engine is a direct port inspired by an MIT-licensed
upstream subtitle sync implementation. This crate does not vendor that source
or archives; the ported implementation is maintained in this GPL-3.0-only plugin.

Re-vendor FFmpeg with:

```sh
cargo xtask ffmpeg revendor --commit <ffmpeg-commit>
```

Use `--source /path/to/FFmpeg` when refreshing from a local checkout. That
refresh rewrites both the human-readable `vendor/ffmpeg/UPSTREAM.md` and the
machine-readable `vendor/ffmpeg/SCRYER_VENDOR_METADATA` file that `build.rs`
uses to pin the upstream revision and invalidate stale FFmpeg build artifacts.

Re-vendor libfvad with:

```sh
cargo xtask vad revendor --commit <libfvad-commit>
```

Use `--source /path/to/libfvad` when refreshing from a local checkout. This
uses the same packed archive and metadata pattern as FFmpeg under
`vendor/libfvad`.
