# TRaSH daily refresh helper

This stdlib-only helper is used only by `trash-daily-refresh.yml`. It computes a
metadata-free fingerprint from the generated rule IDs, Rego sources, facets and
rule behavior fields; source revision, timestamps and pack version do not cause
a refresh on their own. It also rejects a refresh pull request that changes a
converter, native template, workflow, or any path outside the generated pack
artifacts and the current normalized snapshots. Pinned golden inputs remain
outside the automated allowlist.

The scheduled workflow stays inactive until an operator sets
`TRASH_REFRESH_ENABLED` to `true`. Activation also requires the
`TRASH_REFRESH_TOKEN` and `TRASH_REFRESH_SIGNING_KEY` secrets, plus the
`TRASH_REFRESH_ALLOWED_SIGNERS`, `TRASH_REFRESH_BOT_LOGIN`, and immutable
40-character `TRASH_HOST_VALIDATION_SHA` repository variables. The host SHA
must include the ignored `trash_pack_sources_evaluate_on_production_runtime`
test; the workflow checks out that exact host revision and runs the test against
the generated pack before it can commit a refresh.

The write credential and signing key are operator-provisioned credentials. If
any required credential, signer configuration, or host harness is absent, the
workflow stops before it creates a commit, pull request, tag, or publication.
There is no unsigned fallback.

The helper never fetches upstream data, creates commits, pushes branches, or
publishes a release. Those actions remain in the workflow and use the normal
signed `release-publish-tags --resume` path after a generated-only pull request
merges. Resume verifies any existing signed tags against the merged commit and
pushes only missing refs. It does not overwrite an artifact or create another
version for the same merged change. Successful tag creation only dispatches
publication; the downstream publication workflow must also succeed. If that
workflow fails after its trigger tag exists, an operator reruns the failed
workflow rather than creating another trigger or version.
