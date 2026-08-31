#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=release-plugin-v3-identity.sh
source "${script_dir}/release-plugin-v3-identity.sh"

readonly TEST_REPOSITORY='scryer-media/scryer-plugins'
readonly TEST_RELEASE_REF='refs/tags/plugins-v3/release/20260831-abcdef0'
readonly EXPECTED_IDENTITY="https://github.com/${TEST_REPOSITORY}/.github/workflows/release-plugin-v3.yml@${TEST_RELEASE_REF}"

actual_identity="$(release_workflow_identity "${TEST_REPOSITORY}" "${TEST_RELEASE_REF}")"
[[ "${actual_identity}" == "${EXPECTED_IDENTITY}" ]]
require_release_publish_context push "${TEST_RELEASE_REF}"

if require_release_publish_context workflow_dispatch "${TEST_RELEASE_REF}" 2>/dev/null; then
  printf 'unexpectedly accepted workflow_dispatch publication context\n' >&2
  exit 1
fi

for rejected_ref in \
  'refs/heads/main' \
  'refs/pull/123/merge' \
  'refs/tags/plugins-v3/email/v0.1.14' \
  'refs/tags/plugins-v3/release/' \
  ''
do
  if require_release_trigger_ref "${rejected_ref}" 2>/dev/null; then
    printf 'unexpectedly accepted release ref %s\n' "${rejected_ref:-<empty>}" >&2
    exit 1
  fi
done
