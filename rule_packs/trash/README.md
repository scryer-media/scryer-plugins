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
`audiophile`, `efficient`, or `compatible`. Optional boolean keys in
`profile.scoring_overrides` disable individual named contributions when false.
The conversion requires parser-owned `release.normalized_tokens`; all former
guide-fact detection is performed inside Rego. The locale templates additionally
consume the existing parsed release/context fields. Generic native replacement
templates in `templates/native-*.rego` are included verbatim during generation.
French variants are mutually exclusive; core and native templates are enabled
for the baseline phase, while locale templates are optional additional rules.
