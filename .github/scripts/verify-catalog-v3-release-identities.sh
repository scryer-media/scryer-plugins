#!/usr/bin/env bash

set -euo pipefail

catalog_path="${1:?catalog path is required}"
repository="${2:?repository is required}"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=release-plugin-v3-identity.sh
source "${script_dir}/release-plugin-v3-identity.sh"

work_root="$(mktemp -d)"
trap 'rm -rf "${work_root}"' EXIT

jq -r '
  .plugins[]
  | select(.support_tier == "official")
  | . as $plugin
  | $plugin.releases[0] as $release
  | select($release.max_scryer_version == null)
  | $release.artifacts[]
  | [$plugin.id, $release.version, .url, .signature_url]
  | @tsv
' "${catalog_path}" > "${work_root}/selected-artifacts.tsv"

if [[ ! -s "${work_root}/selected-artifacts.tsv" ]]; then
  printf 'catalog does not select any official plugin artifacts\n' >&2
  exit 1
fi

verify_catalog_artifact() {
  local plugin_id="${1:?plugin id is required}"
  local version="${2:?version is required}"
  local artifact_url="${3:?artifact URL is required}"
  local signature_url="${4:?signature URL is required}"
  local artifact_dir
  artifact_dir="$(mktemp -d "${work_root}/artifact.XXXXXX")"
  local artifact_path="${artifact_dir}/artifact"
  local downloaded_bundle="${artifact_dir}/bundle.download"
  local bundle_path="${artifact_dir}/bundle.json"

  printf 'Verifying tag-bound identity for %s %s: %s\n' \
    "${plugin_id}" "${version}" "${artifact_url}"
  curl -fsSL "${artifact_url}" -o "${artifact_path}"
  curl -fsSL "${signature_url}" -o "${downloaded_bundle}"
  case "${signature_url}" in
    *.zst) zstd -dc "${downloaded_bundle}" > "${bundle_path}" ;;
    *) cp "${downloaded_bundle}" "${bundle_path}" ;;
  esac
  verify_release_blob_tag_bound \
    "${artifact_path}" \
    "${bundle_path}" \
    "${repository}"
}

readonly parallelism=8
declare -a active_pids=()
failed=0

wait_for_slot() {
  while (( ${#active_pids[@]} >= parallelism )); do
    if ! wait -n "${active_pids[@]}"; then
      failed=1
    fi
    local -a remaining=()
    local pid
    for pid in "${active_pids[@]}"; do
      if kill -0 "${pid}" 2>/dev/null; then
        remaining+=("${pid}")
      fi
    done
    active_pids=("${remaining[@]}")
  done
}

while IFS=$'\t' read -r plugin_id version artifact_url signature_url; do
  wait_for_slot
  if (( failed )); then
    break
  fi
  verify_catalog_artifact \
    "${plugin_id}" \
    "${version}" \
    "${artifact_url}" \
    "${signature_url}" &
  active_pids+=("$!")
done < "${work_root}/selected-artifacts.tsv"

for pid in "${active_pids[@]}"; do
  if ! wait "${pid}"; then
    failed=1
  fi
done

(( ! failed ))
