# TRaSH Guides scoring pack

This directory converts a pinned raw TRaSH Guides tree into the
single `trash-guides-scoring-pack` asset. The saved snapshots are immutable
review inputs: `core-snapshot.json` preserves release-group source order,
matcher, facet and context; `detection-snapshot.json` preserves the curated
matching and locale membership tables that the readable Rego sources consume.

Run commands from this directory:

```sh
go run . fetch --revision <branch-or-sha> --output snapshot/upstream-raw.json
go run . distill --snapshot snapshot/upstream-raw.json --output-dir snapshot
go run . generate --snapshot snapshot/core-snapshot.json --output-dir .. --pack-version 1.0.0
go run . check --snapshot snapshot/core-snapshot.json --output-dir .. --pack-version 1.0.0
```

`generate` writes `../trash-scoring.json` and
`../trash-scoring-coverage.json` through same-directory atomic renames.
`check` regenerates in memory and reports a missing or different artifact
without writing anything. Use `--previous-snapshot` to record added and
removed compiled group rows in the coverage report.

`fetch` deliberately requires an explicit immutable source, either a local
reviewed snapshot or an HTTPS URL:

```sh
go run . fetch --source /path/to/core-snapshot.json --output snapshot/core-snapshot.json
```

It validates before renaming, so malformed input cannot replace the saved
snapshot. It does not invoke the application sync command, run Cargo, fetch
implicitly, or publish an asset.

The Rego source uses `profile.scoring_persona` with one of `balanced`,
`audiophile`, `efficient`, or `compatible`. The five optional booleans in
`profile.scoring_overrides` are `allow_x265_non4k`, `block_dv_without_fallback`,
`prefer_compact_encodes`, `prefer_lossless_audio`, and `block_upscaled`. They
retain persona defaults when unset. Individual weights such as `group_gold`
are customized in the rule source; they are not profile override fields. `block_upscaled`
defaults to true; `block_dv_without_fallback` defaults to false. Their −10,000
penalties remain recoverable through the complete score. The required-language
bonus compares canonical codes in `profile.required_audio_languages` and
`release.languages_audio`, and requires every requested language.
The conversion requires parser-owned `release.normalized_tokens`; all former
guide-fact detection is performed inside Rego. The locale templates additionally
consume the existing parsed release/context fields. Generic native replacement
templates in `templates/native-*.rego` are included verbatim during generation.
French variants are mutually exclusive; core and native templates are enabled
for the baseline phase, while locale templates are optional additional rules.

## Review regression checks

`go test ./...` covers the converter, serialized score-change reporting, and
pinned upstream membership. The native golden snapshots remain unchanged;
their membership comparisons project the new source-specific anime rows back
to the old collapsed representation. Separate tests assert Arid's BD silver /
WEB gold tiers and AO's German BD tier 1 / WEB tier 2.

`validation/production_review.rs` contains 19 scoring scenarios for the existing
Scryer offline evaluator harness. To run them with the host's locked dependencies,
temporarily append an `include!` with that file's absolute path to
`crates/scryer-rules/src/trash_pack_validation.rs` in an isolated Scryer worktree.
Set `SCRYER_TRASH_PACK` to the generated pack's absolute path and run
`cargo nextest run --locked -p scryer-rules --lib -E
'test(trash_pack_review_regressions) | test(trash_pack_sources_evaluate_on_production_runtime)'
--run-ignored only --no-fail-fast`, then remove the temporary include.

PR #71 validation: Go tests, deterministic artifact checks, and both production
evaluator tests passed (run `b070f418-b0c7-4089-99ca-99cccf5d2312`). The runtime
cases cover source-specific tiers, remux exclusion, disc spelling, missing
groups, upscaling detection, explicit opt-out, and no duplicate tier scores.
