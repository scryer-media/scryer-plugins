# Archive Extraction

Optional Scryer archive extraction plugin for ZIP, RAR, and 7z.

This plugin is the intended license boundary for UnRAR-restricted extraction support. Scryer core keeps TAR, PAR2, and transport compression handling; RAR, ZIP, and 7z extraction belongs here.

Current support:

- ZIP extraction for stored/deflated archives
- 7z extraction for LZMA/LZMA2, AES, BZip2, Deflate, PPMD, and Copy methods
- RAR extraction through `weaver-unrar` using Scryer's host AES/CRC imports

Zstandard-compressed 7z archives are not supported yet.

Scryer core performs PAR2 verification, placement normalization, and repair
before invoking this plugin. Extraction output is always written to the
caller-provided `output_dir`.
