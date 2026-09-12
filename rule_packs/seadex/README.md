# SeaDex scoring pack

The full-snapshot pack adds **12.9 MiB RSS in the fresh-load runtime test**,
below the 15 MiB target. Validation followed by loading in the same process
retains **16.0 MiB additional RSS**, so the target is not met for that path.
These are local measurements, not a process-wide memory guarantee; concurrent
searches add evaluator state. The pack uses pure Rego with no SeaDex-specific
host functions.

This directory builds a schema-v1 community rule pack from a SeaDex snapshot.
It uses only the Go standard library. The generated pack has one installed
anime ruleset with four mutually exclusive scoring branches: strict/tolerant
and best/listed.

## Build commands

Run commands from this directory:

```sh
go run . fetch --output snapshot.json.gz
go run . generate --snapshot snapshot.json.gz --output-dir .. --pack-version 1.0.1
go run . check --snapshot snapshot.json.gz --output-dir .. --pack-version 1.0.1
```

`fetch` retrieves every SeaDex page and writes a normalized snapshot. It
rejects incomplete pagination or malformed required fields before replacing an
existing output. The output path may end in `.json` or `.json.gz`.

`generate` works entirely offline when given a saved normalized snapshot. It
writes these deterministic artifacts:

- `seadex-scoring.json` — installable community pack.
- `seadex-scoring.rego` — readable generated policy.
- `seadex-coverage.json` — snapshot checksum, converter revision, source
  additions/removals, matching coverage, skipped records, ambiguities, and
  stale overrides.

Use `--previous-snapshot /path/to/previous.json.gz` with `generate` or
`check` to include added and removed SeaDex entry and release IDs in coverage.
Use `--overrides /path/to/overrides.json` to apply reviewed aliases or
exclusions. No overrides file is committed; the default is an empty override
set. `check` regenerates in memory and exits nonzero when any artifact is
missing or differs; it does not write artifacts.

`--pack-version` sets the immutable SemVer version written to both the pack
and coverage report. It defaults to `1.0.0`; the committed pack is `1.0.1`,
which marks the pack non-customizable (`customizable: false`). Supply the
same explicit version to `generate` and `check`; a different version makes
`check` report the existing artifacts as outdated.

For a snapshot refresh, review the coverage changes against
`--previous-snapshot` and any reviewed overrides, choose the next pack
version, then regenerate and check with that exact `--pack-version`. The
converter never fetches automatically or publishes a pack. Publication after
merge remains a separately authorized operator action.

## Snapshot and overrides

The normalized snapshot preserves SeaDex entry IDs, AniList IDs, timestamps,
notes, theoretical-best text, release IDs, release groups, best flags, and
file names and sizes, including excluded files. Notes and theoretical-best
text are retained for review only; they are never converted into scoring rules.

Overrides are intentionally narrow and keyed by a SeaDex release ID:

```json
{
  "schema_version": 1,
  "releases": {
    "seadex-release-id": {
      "exclude": false,
      "aliases": ["Reviewed complete-season name"]
    }
  }
}
```

`exclude: true` removes that release from matching. An alias is explicit
release-name evidence, including a reviewed batch alias; it does not turn
other releases by the same group into recommendations. The coverage report
lists override keys whose release IDs no longer exist upstream.

## Matching and scoring

The rule applies only to the `anime` facet and requires `input.context.is_anime`.
It does not inspect protocol, info hash, or listing metadata, so it works with
the canonical input when `input.release.extra` is empty.

Strict matching compares lower-cased video basenames after path removal,
recognized video-extension removal, and basic whitespace normalization. It
retains episode, season, version, source, resolution, codec, and checksum
tokens.

Source names and aliases containing `İ` or `Σ` are conservatively excluded
because Go and the runtime can lowercase those characters differently. The
coverage reason is `unsupported_unicode_case_mapping`; no records in the
included snapshot require this exclusion. Other non-Latin strict names remain
eligible.

The policy stores repeated literal filename text once, with exact correlated
tuples for numeric tokens and CRCs. Constant captures are checked explicitly;
season, episode, version, resolution, codec and checksum values cannot be
recombined across records. Names that do not share a template remain complete
framed strings. Both indexes select buckets by Unicode-scalar length, then
compare exact evidence within that bucket. This is reversible encoding, not a
hash or a broadened filename pattern. One cached match result supplies all four
score branches for each candidate.

Tolerant matching runs only after a strict miss. It normalizes separators and
brackets, removes a standalone bracketed CRC, and requires a nonempty declared
release group that visibly occurs at the beginning or end of the raw title.
The source filename must carry the same edge group. Tolerant matching skips
non-ASCII evidence to avoid collapsing non-Latin titles. It never treats every
release from a recommended group as recommended.

Samples, extras, trailers, non-video files, generic numeric-only names, and
unreviewed whole-season inference are excluded. Duplicate recommendations can
collapse only when AniList ID, raw group spelling, score level, signature, and
file size agree. Conflicts, CRC-only variants, different sizes, and normalized
group ambiguities receive no bonus and are reported. A strict collision also
prevents tolerant fallback.

An accepted release yields exactly one entry:

- `+400` for a SeaDex best release.
- `+200` for another listed release.

The score codes distinguish strict and tolerant best/listed matches. Existing
quality-profile restrictions remain in effect.

The rendered source has one ruleset and four scoring branches, plus shared
normalization, decoding and collision helpers. The helper count exceeds the
original 10–15 estimate; it does not create rules per anime or per release.

## Snapshot and generated coverage

The fetched snapshot contains 2,850 entries, 8,852 unique release IDs, 9,328
release occurrences, and 140,406 file occurrences. The compressed snapshot is
2,218,650 bytes.

The generated policy retains 54,936 strict and 44,503 tolerant accepted keys.
Its physical representation contains 20,450 direct records in 350 buckets
(maximum 218 records) and 6,423 templates in 261 buckets (maximum 125 records),
with 6,605 tuple chunks. Strict collision blockers are included in these
physical counts; tolerant conflicts remain excluded from accepted lookups.
The full Rego source is 2,908,119 bytes and 43,833 logical lines.

Coverage reports 7,777 included entry/release pairs, 37,613 file or branch
skip records, and 13,904 ambiguous signature keys. These counts are not
disjoint: one source record can contribute to more than one category.

## Tests and engine validation

Run the Go unit tests with:

```sh
go test ./...
```

The offline engine harness copies only Scryer's rule crate and its existing
dependency declarations into temporary storage, then uses its package rewrite,
validation, build, and evaluation path:

```sh
go run ./validation \
  --scryer /path/to/scryer \
  --jobs /path/to/jobs.json \
  --report /path/to/report.json \
  --max-added-rss-mib 15
```

This default installation-phase command currently reports passing correctness
but fails the 15 MiB RSS gate. After validating the source, add
`--memory-phase runtime --source ../seadex-scoring.rego` to measure the
already-validated runtime path, which passes the gate in the recorded run.

The RSS gate requires exactly one job in a fresh test process. It checks both
loaded-engine and post-batch resident memory against the pre-source baseline;
passing correctness checks alone does not pass this gate. The default
`--memory-phase installation` includes rule validation and then engine loading
in the same process. `--memory-phase runtime` measures loading already-validated
rules; always run the installation validation separately as well. Use
`--source ../seadex-scoring.rego` for runtime measurement to avoid counting JSON
pack-envelope parsing. Concurrent evaluators and peak live allocations are
reported separately. Omit the RSS flag for correctness-only validation.

Reports are stored under `validation/`. To regenerate the engine corpus:

```sh
SEADEX_ENGINE_CORPUS_DIR=/tmp/seadex-corpus \
SEADEX_FULL_SNAPSHOT=snapshot.json.gz \
go test -run 'Test(Encoding)?EngineCorpus' -count=1
```

Set `SEADEX_EXHAUSTIVE_CORPUS=1` as well to enumerate every strict and tolerant
accepted key and every strict collision. This produces 107,058 cases for the
included snapshot. Use the smaller sampled corpus for memory comparisons;
preloading the exhaustive cases changes the process's allocation baseline.

All 107,058 exhaustive cases and all 58 independent adversarial cases pass,
including equality between fresh and reused evaluator results, with no
unexpected bonuses or score stacking. Go unit tests, `go vet`, and deterministic
regeneration checks also pass.

The corpus writes harness job files whose source paths are consumed by the
validation command above. It includes 40 independent matching fixtures,
18 independently authored encoding cases, and 1,227 sampled full-snapshot
cases. The exhaustive corpus also restores one video extension on each stored
strict stem, including upstream filenames with a doubled extension.

The local host integration passes Rust formatting, Clippy for all
targets/features of both changed crates, and all 3,441 application/rules
Nextest tests. Two existing ignored tests remain excluded:
`profile_real_media_root_nfo_parsing` and
`bench_eval_rule_scaling_against_legacy_query_loop`. No test was disabled.

The catalog manifest sets `min_scryer_version` to `0.20.0`. That host version
must include the general rule-engine changes used for the measurements below;
the version gate alone does not implement those changes.

The separately validated host integration raises policy limits to 16 MiB and
200,000 lines while retaining the default 1,024-column lexer limit. It reuses
one evaluator for the sequential candidates and streamed pages within each
search, with a separate owned rules snapshot for each search. This work did
not modify the original checkout or running instances. The full pack requires
these host source-limit changes; it exceeds the original 1 MiB / 20,000-line
limits. No dependency, input schema, database, or indexer changes are needed.

Two general allocation fixes also apply to every community rule: package
rewriting builds one output buffer, and validation inspects the already-parsed
module instead of creating a second syntax tree. There are no SeaDex-specific
host functions or native lookup helpers.

Measured with the full snapshot, the final 1,227-case reused runtime batch took
584.95 ms with 0.872 ms p95 evaluation; fresh evaluators took 789.87 ms.
Package rewriting took 1.48 ms, parse/build took 4.92 ms, and first evaluation
took 0.91 ms. These are local optimized rule-engine measurements, not
end-to-end search latency. See `validation/runtime-results.json`.

The memory measurement subtracts the 3.34 MiB process baseline before source
loading. It includes shared source/data, engine state, and allocator retention;
it is not a per-release allocation or a measured RSS peak.

| Measurement | Additional RSS |
| --- | ---: |
| Fresh-load runtime, engine loaded | 10.81 MiB |
| Fresh-load runtime, after reused batch | 12.88 MiB |
| Fresh-load runtime, four prepared evaluators | 13.89 MiB |
| Fresh-load runtime, sixteen prepared evaluators | 20.53 MiB |
| Validation then loading, engine loaded | 14.78 MiB |
| Validation then loading, after reused batch | 16.02 MiB |

Evaluators share immutable policy and retain separate mutable evaluation state.
The runtime report skips validation explicitly; correctness is established by
the separate installation-phase run in `validation/full-results.json`, which
passes all cases but fails the RSS gate. That run spent 5.78 ms in validation
and 4.33 ms in parse/build. It does not measure the complete application import
request or JSON pack-envelope parsing. The 15 MiB limit therefore holds for the
recorded fresh-load runtime case, not every lifecycle or concurrency scenario.

The canonical host calls `eval_rule`; parsing and building occur as the rules
engine is built, while lazy preparation occurs on a fresh evaluator. The
harness measures fresh evaluators and sequential batch reuse separately.

The pack is a regenerated snapshot, not a live synchronization mechanism. A
name match is release-name evidence only; it does not verify an identical
torrent or Usenet payload.
