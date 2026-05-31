# Test Fixtures

- `sine440_stereo.ac3` is copied from `oxideav-ac3` 0.0.6 test fixtures.
  That crate is MIT licensed; the fixture is used here only as a small AC-3
  decode smoke test input.
- `test-data/` contains generated speech media and subtitles for deterministic
  subtitle sync tests. The checked-in media files are static fixtures; tests
  must not call external speech services.
