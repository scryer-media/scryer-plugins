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
- decode AC-3, E-AC-3, and TrueHD/MLP windows to interleaved f32 little-endian
  PCM
- use lightweight frame sniffing to route packets before decode
- carry a pinned FFmpeg decoder-source snapshot as the reference/porting base
- keep Symphonia as the expected container demux and stream-selection layer in
  the Scryer app

Current decoder status:

- AC-3 / E-AC-3: decoded through the OxideAV AC-3 family decoder
- TrueHD / MLP: decoded through the `truehd` decoder
- DTS: packet routing is implemented; PCM decode still needs the vendored
  FFmpeg DTS/DCA decoder to be ported or compiled into a Wasm-safe backend

The exported response uses base64-encoded interleaved `f32le` PCM so the host
can feed the same VAD/alignment path used by the in-process subtitle sync code.
