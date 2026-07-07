# Archive Extraction

Optional Scryer archive extraction plugin for ZIP, RAR, and PAR2 verify/repair.

This plugin is the intended license boundary for UnRAR-restricted extraction support. Scryer core keeps native 7z, TAR, and transport compression handling; RAR, ZIP, and PAR2 archive work belongs here.

Current support:

- ZIP extraction for stored/deflated archives
- RAR extraction through `weaver-unrar` using Scryer's host AES/CRC imports
- PAR2 verification and repair through `weaver-par2`

PAR2 repair runs against Scryer's writable copy-on-write staging directory. The
plugin does not mutate the completed-download source mount in place.
