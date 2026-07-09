# Archive Extraction

Optional Scryer archive extraction plugin for ZIP, RAR, and PAR2 verify/repair.

This plugin is the intended license boundary for UnRAR-restricted extraction support. Scryer core keeps native 7z, TAR, and transport compression handling; RAR, ZIP, and PAR2 archive work belongs here.

Current support:

- ZIP extraction for stored/deflated archives
- RAR extraction through `weaver-unrar` using Scryer's host AES/CRC imports
- PAR2 verification and repair through `weaver-par2`

PAR2 repair mutates the `RepairThenExtract.source_dir` that Scryer passes to the
plugin. Scryer uses the completed-download source only after a successful write
probe; otherwise it passes a writable destination-side repair staging directory.
Extraction output is always written to the caller-provided `output_dir`.
