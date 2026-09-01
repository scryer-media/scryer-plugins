#!/usr/bin/env bash

set -euo pipefail

readonly RELEASE_PLUGIN_V3_WORKFLOW_PATH='.github/workflows/release-plugin-v3.yml'
readonly RELEASE_PLUGIN_V3_WORKFLOW_NAME='release-plugin-v3'
readonly RELEASE_PLUGIN_V3_OIDC_ISSUER='https://token.actions.githubusercontent.com'

require_release_trigger_ref() {
  local ref="${1:-}"
  case "${ref}" in
    refs/tags/plugins-v3/release/?*) return 0 ;;
    *)
      printf 'Publishing requires refs/tags/plugins-v3/release/*; got %s\n' "${ref:-<empty>}" >&2
      return 1
      ;;
  esac
}

require_release_publish_context() {
  local event_name="${1:-}"
  local ref="${2:-}"
  if [[ "${event_name}" != 'push' ]]; then
    printf 'Publishing requires the push event; got %s\n' "${event_name:-<empty>}" >&2
    return 1
  fi
  require_release_trigger_ref "${ref}"
}

release_workflow_identity() {
  local repository="${1:?repository is required}"
  local ref="${2:?ref is required}"
  require_release_trigger_ref "${ref}"
  printf 'https://github.com/%s/%s@%s\n' \
    "${repository}" \
    "${RELEASE_PLUGIN_V3_WORKFLOW_PATH}" \
    "${ref}"
}

verify_release_blob() {
  local artifact="${1:?artifact is required}"
  local bundle="${2:?bundle is required}"
  local repository="${3:?repository is required}"
  local ref="${4:?ref is required}"
  local identity
  identity="$(release_workflow_identity "${repository}" "${ref}")"
  cosign verify-blob \
    --bundle "${bundle}" \
    --certificate-identity "${identity}" \
    --certificate-oidc-issuer "${RELEASE_PLUGIN_V3_OIDC_ISSUER}" \
    --certificate-github-workflow-name "${RELEASE_PLUGIN_V3_WORKFLOW_NAME}" \
    --certificate-github-workflow-ref "${ref}" \
    --certificate-github-workflow-repository "${repository}" \
    --certificate-github-workflow-trigger push \
    "${artifact}"
}

verify_release_blob_tag_bound() {
  local artifact="${1:?artifact is required}"
  local bundle="${2:?bundle is required}"
  local repository="${3:?repository is required}"
  local identity_regexp
  identity_regexp="^https://github.com/${repository//./\\.}/\\.github/workflows/release-plugin-v3\\.yml@refs/tags/plugins-v3/release/.+$"
  cosign verify-blob \
    --bundle "${bundle}" \
    --certificate-identity-regexp "${identity_regexp}" \
    --certificate-oidc-issuer "${RELEASE_PLUGIN_V3_OIDC_ISSUER}" \
    --certificate-github-workflow-name "${RELEASE_PLUGIN_V3_WORKFLOW_NAME}" \
    --certificate-github-workflow-repository "${repository}" \
    --certificate-github-workflow-trigger push \
    "${artifact}"
}

if [[ "${BASH_SOURCE[0]:-}" == "$0" ]]; then
  case "${1:-}" in
    require-ref)
      require_release_trigger_ref "${2:-}"
      ;;
    require-context)
      require_release_publish_context "${2:-}" "${3:-}"
      ;;
    identity)
      release_workflow_identity "${2:-}" "${3:-}"
      ;;
    *)
      printf 'usage: %s {require-ref REF|require-context EVENT REF|identity REPOSITORY REF}\n' "$0" >&2
      exit 2
      ;;
  esac
fi
