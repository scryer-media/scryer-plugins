# REMUX Streaming Copy

Experimental, unpublished media transcoder plugin.

This crate is intentionally not part of the official public catalog. It stages
the future transcoder plugin contract Scryer needs before the SDK/runtime grows
the required media IO and long-running job hooks.

Initial target:

- accept Scryer-managed media input and output references
- transcode REMUX-grade sources into streaming copies
- support AV1, H.264, and HEVC target video codecs
- keep high-quality defaults for Blu-ray/DVD archive sources
- preserve chapters, text subtitles, HDR metadata, and useful audio layout when
  the selected container/encoder can carry them

The descriptor exposes the future `media_transcoder` provider shape. The
planning export is implemented and deterministic. The run export currently
returns `unsupported` until Scryer exposes the host facilities listed in the
descriptor:

- host-provided readable media streams
- host-provided writable output streams
- long-running progress/checkpoint reporting
- filesystem scratch space with explicit quotas
- `exception-handling` WASM feature selection for libaom/SJLJ builds

No SDK, ABI, catalog schema, or publishing logic is changed by this crate.
