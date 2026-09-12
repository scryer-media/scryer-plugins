#!/usr/bin/env bash
# Provision the credentials and variables that activate the automated rule-pack
# refresh workflows (trash-daily-refresh, seadex-daily-refresh, rule-pack-publish).
#
# Usage:
#   RULE_PACK_REFRESH_TOKEN=<fine-grained PAT> \
#     .github/scripts/provision-rule-pack-refresh.sh \
#       --host-sha <40-hex scryer commit> [--repo scryer-media/scryer-plugins]
#
# The token must belong to the account that will author refresh pull requests
# and push release tags. Fine-grained permissions on the plugins repository:
# Contents: read and write, Pull requests: read and write, Metadata: read.
# The script generates a fresh ed25519 signing key, stores it as a secret with
# the token, records the matching allowed-signers line, the bot login, and the
# host validation commit as variables, enables auto-merge on the repository,
# and finally flips RULE_PACK_REFRESH_ENABLED to true. It never prints the
# token or the private key.

set -euo pipefail

repo="scryer-media/scryer-plugins"
host_sha=""
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) repo="$2"; shift 2 ;;
    --host-sha) host_sha="$2"; shift 2 ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done

token="${RULE_PACK_REFRESH_TOKEN:-}"
test -n "$token" || { echo "RULE_PACK_REFRESH_TOKEN must be set in the environment" >&2; exit 2; }
[[ "$host_sha" =~ ^[0-9a-f]{40}$ ]] || { echo "--host-sha must be an immutable 40-hex scryer commit" >&2; exit 2; }
command -v gh >/dev/null || { echo "gh is required" >&2; exit 2; }
command -v ssh-keygen >/dev/null || { echo "ssh-keygen is required" >&2; exit 2; }

bot_login="$(GH_TOKEN="$token" gh api user --jq .login)"
test -n "$bot_login" || { echo "the token does not resolve to a GitHub user" >&2; exit 1; }
GH_TOKEN="$token" gh api "repos/$repo" --jq .full_name >/dev/null || { echo "the token cannot read $repo" >&2; exit 1; }
gh api "repos/scryer-media/scryer/commits/$host_sha" --jq .sha >/dev/null || { echo "$host_sha is not a commit on scryer-media/scryer" >&2; exit 1; }
if ! gh api "repos/scryer-media/scryer/contents/crates/scryer-rules/src/trash_pack_validation.rs?ref=$host_sha" --jq .path >/dev/null 2>&1; then
  echo "$host_sha lacks crates/scryer-rules/src/trash_pack_validation.rs; pick a commit with the production-runtime validation test" >&2
  exit 1
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
ssh-keygen -q -t ed25519 -N "" -C "rule-pack-refresh-bot" -f "$workdir/signing-key"
public_key="$(cat "$workdir/signing-key.pub")"
allowed_signers="rule-pack-refresh-bot@users.noreply.github.com namespaces=\"git\" $public_key"

echo "Provisioning $repo for refresh bot @$bot_login"
gh secret set RULE_PACK_REFRESH_TOKEN --repo "$repo" --body "$token"
gh secret set RULE_PACK_REFRESH_SIGNING_KEY --repo "$repo" < "$workdir/signing-key"
gh variable set RULE_PACK_REFRESH_ALLOWED_SIGNERS --repo "$repo" --body "$allowed_signers"
gh variable set RULE_PACK_REFRESH_BOT_LOGIN --repo "$repo" --body "$bot_login"
gh variable set RULE_PACK_HOST_VALIDATION_SHA --repo "$repo" --body "$host_sha"
gh api -X PATCH "repos/$repo" -F allow_auto_merge=true --jq .allow_auto_merge >/dev/null
gh variable set RULE_PACK_REFRESH_ENABLED --repo "$repo" --body "true"

cat <<MSG

Done. Refresh workflows are enabled on $repo.

Optional: add this public key to @$bot_login as an SSH *signing* key so GitHub
shows refresh commits as Verified (the workflows verify signatures themselves
against RULE_PACK_REFRESH_ALLOWED_SIGNERS regardless):

  $public_key

Trigger a first run with:
  gh workflow run trash-daily-refresh.yml --repo $repo
  gh workflow run seadex-daily-refresh.yml --repo $repo
MSG
