# Rule-pack refresh helper

Shared Go helper behind the automated TRaSH and SeaDex refresh workflows. It
knows one **profile** per pack and answers three questions the workflows ask:

| Command | Purpose |
| --- | --- |
| `fingerprint --profile <p> --pack <json>` | Semantic fingerprint of a generated pack (rules, Rego, scoring policy), ignoring version, snapshot metadata, and other churn. Equal fingerprints mean "no refresh needed". |
| `next-version --profile <p> --pack <json>` | The next patch version for the current pack. |
| `verify-diff --profile <p> --base <rev>` | Fails unless every file changed between `<rev>` and `HEAD` is on the profile's generated-artifact allowlist. Guards both the refresh PR and the publication step. |

Profiles:

| Profile | Rule pack id | Allowed paths |
| --- | --- | --- |
| `trash` | `trash-guides-scoring-pack` | `rule_packs/trash-scoring.json`, `rule_packs/trash-scoring-coverage.json`, `rule_packs/trash/snapshot/*`, `rule_packs/trash/generated/*` |
| `seadex` | `seadex-scoring-pack` | `rule_packs/seadex-scoring.json`, `rule_packs/seadex-scoring.rego`, `rule_packs/seadex-coverage.json`, `rule_packs/seadex/snapshot.json.gz` |

Go sources, workflow files, and READMEs are never allowlisted, so a refresh PR
can only ever carry generated content.

```bash
cd rule_packs/refresh
go test ./...
go run . fingerprint --profile seadex --pack ../seadex-scoring.json
go run . verify-diff --profile trash --base origin/main
```

## Workflows

| Workflow | Trigger | What it does |
| --- | --- | --- |
| `trash-daily-refresh.yml` | daily 04:23 UTC, manual (`upstream_revision`) | fetch + distill + generate in `rule_packs/trash`; opens/updates the `feature/trash-daily-refresh` PR when the fingerprint changes |
| `seadex-daily-refresh.yml` | daily 04:53 UTC, manual | `fetch` + `refresh` in `rule_packs/seadex`; opens/updates the `feature/seadex-daily-refresh` PR when the Rego changes |
| `rule-pack-publish.yml` | `pull_request: closed` on `main` | when a refresh PR authored by the bot merges, re-verifies the merged diff and runs `cargo xtask release-publish-tags --resume --pr <n> --rule-pack-id <id>`, which pushes the signed `rule-packs/<id>/v<version>` tag and the `plugins-v3/release/*` trigger tag |

Both refresh workflows share the same shape:

1. Preflight the credentials and variables below (fail loudly, never skip).
2. Check out `main` with the bot token and record the base SHA.
3. Run the helper and converter unit tests.
4. Fetch upstream, regenerate, and stop when the semantic fingerprint is unchanged.
5. Validate the generated pack on the pinned production Rego runtime
   (`.github/actions/rule-pack-host-validation`, which runs scryer's
   `trash_pack_sources_evaluate_on_production_runtime` test against the pack).
6. `verify-diff` against the base SHA.
7. Commit with the bot's SSH signing key, push `feature/<pack>-daily-refresh`,
   open or update the PR, and enable squash auto-merge
   (`.github/actions/rule-pack-refresh-pr`).

The refresh PR then runs the normal plugin CI. Once required checks pass,
auto-merge squashes it into `main`, and `rule-pack-publish.yml` publishes the
new pack version through the same signed tag path a human release uses.
Only a merged PR whose author is `RULE_PACK_REFRESH_BOT_LOGIN`, whose head is
one of the two refresh branches, and whose merged diff is still allowlisted
gets published; the workflow is a no-op for every other PR.

## Activation

Nothing runs until `RULE_PACK_REFRESH_ENABLED` is `true`. Provision everything
with `.github/scripts/provision-rule-pack-refresh.sh` (see its header), which
sets the values below and flips the switch last.

| Kind | Name | Value |
| --- | --- | --- |
| secret | `RULE_PACK_REFRESH_TOKEN` | fine-grained PAT of the bot account: Contents RW, Pull requests RW, Metadata R on this repository. A PAT (not `GITHUB_TOKEN`) is required so the refresh PR triggers CI and the publication tag push triggers the release workflow. |
| secret | `RULE_PACK_REFRESH_SIGNING_KEY` | ed25519 private key used to sign refresh commits and publication tags |
| var | `RULE_PACK_REFRESH_ALLOWED_SIGNERS` | one `allowed_signers` line: `rule-pack-refresh-bot@users.noreply.github.com namespaces="git" <public key>` |
| var | `RULE_PACK_REFRESH_BOT_LOGIN` | GitHub login of the PAT owner; publication only trusts PRs authored by this login |
| var | `RULE_PACK_HOST_VALIDATION_SHA` | immutable 40-hex scryer commit that contains `crates/scryer-rules/src/trash_pack_validation.rs`; pick it from the release branch that ships the packs |
| var | `RULE_PACK_REFRESH_ENABLED` | `true` to run; unset or anything else skips every job |

The repository must also allow auto-merge (the script enables it). Rulesets on
`main` stay in force: the refresh PR has to pass the same required checks as
any other PR.

## Re-running and troubleshooting

- A failed refresh is safe to re-run from the Actions tab; the branch is
  force-pushed with lease and the PR is updated in place.
- If the publication job fails after the tags exist, rerun it: the xtask
  `--resume` path reuses existing signed tags and only pushes what is missing.
- `verify-diff` failures mean someone pushed a non-generated change onto the
  refresh branch; close the PR and let the next scheduled run recreate it.
- Bumping the pinned scryer commit is a variable change, not a workflow edit.
