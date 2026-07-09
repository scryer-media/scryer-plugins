# Archive Extraction

Optional Scryer archive extraction plugin for ZIP and RAR.

This plugin is the intended license boundary for UnRAR-restricted extraction support. Scryer core keeps native 7z, TAR, PAR2, and transport compression handling; RAR and ZIP extraction belongs here.

Current support:

- ZIP extraction for stored/deflated archives
- RAR extraction through `weaver-unrar` using Scryer's host AES/CRC imports

Scryer core performs PAR2 verification, placement normalization, and repair
before invoking this plugin. Extraction output is always written to the
caller-provided `output_dir`.
