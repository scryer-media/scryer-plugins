# Archive Extraction

Optional Scryer archive extraction plugin for ZIP, RAR, 7z, and XZ, with PAR2 recovery.

This plugin is the intended safety and license boundary for complex extraction. Scryer core keeps TAR and basic transport compression handling; RAR, ZIP, 7z, XZ, and PAR2 belong here.

Current support:

- ZIP extraction for stored/deflated archives
- 7z extraction for LZMA/LZMA2, AES, BZip2, Deflate, PPMD, and Copy methods
- RAR extraction through `unrar-rs` using the host's AES/CRC imports
- XZ stream extraction through C liblzma, statically linked into the WebAssembly artifact
- PAR2 verification, placement normalization, and repair through `par2-rs`

Zstandard-compressed 7z archives are not supported yet.

## Artifact model

The plugin is a **WASI Preview 2 component** implementing
`scryer:archive/archive-extractor@1.0.0` (world vendored at `wit/archive.wit`).
It exports `describe` and `process`, both carrying UTF-8 JSON, and imports one
`crypto` interface for AES-CBC and CRC-32. WASI Preview 2 comes from the host's
linker, which is how the guest sees its preopened directories: a read-only
source, a writable output, and a private `TMPDIR` scratch.

Build target: `wasm32-wasip2`. Compiling liblzma's C sources for that target
needs a WASI SDK sysroot (`WASI_SYSROOT`, plus `CC_wasm32_wasip2` and
`AR_wasm32_wasip2`); CI installs WASI SDK 33.

The earlier `wasm32-wasip1` command artifact is gone. Scryer's archive host is
component-only and rejects a core wasm module with an upgrade diagnostic, so
there is no compatibility mode to fall back to.

## PAR2

PAR2 is not part of the plugin contract — there is no `verify-par2` operation
and no PAR2 report on the response. It is handled **data-driven** instead: every
`Inspect` and `ExtractArchive` looks at the source directory it was given and
uses a `.par2` recovery set if one is sitting there.

- The set is verified, misnamed files are matched by content hash rather than by
  name, and damage is repaired **before** extraction, with a re-verification
  afterwards. A repair that does not verify clean is a failure.
- The source preopen is read-only, so corrected archive inputs are materialized
  in the private `TMPDIR` scratch and removed when the invocation ends.
- When the set protects plain files rather than an archive, the repaired files
  are written into `output_dir` — they are the deliverable.
- Insufficient recovery data fails with a clear message; no recovery set is a
  no-op.

Extraction output is always written to the caller-provided `output_dir`.
