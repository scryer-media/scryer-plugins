# Archive extraction conformance corpus

These fixtures drive `tests/host_conformance.rs` against the archive-extraction
plugin built from the current source tree. The test covers real RAR4, encrypted
RAR4/RAR5, and multivolume RAR5 behavior through the same WASI command and
`extism:host/user` crypto ABI used by Scryer.

The RAR and PAR2 binaries are stored in Git LFS. They are copied from the
existing plugin harness plus the Scryer real-artifact corpus, without duplicate
multivolume data. The encrypted-RAR password is `testpass123`.
