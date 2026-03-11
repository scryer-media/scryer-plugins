#!/usr/bin/env python3

import hashlib
import json
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
REGISTRY_PATH = REPO_ROOT / "registry.json"
DIST_DIR = REPO_ROOT / "dist"
RAW_PREFIX = "https://raw.githubusercontent.com/scryer-media/scryer-plugins/main/dist/"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    registry = json.loads(REGISTRY_PATH.read_text())
    errors: list[str] = []

    for plugin in registry.get("plugins", []):
        plugin_id = plugin["id"]
        if plugin.get("builtin", False):
            continue

        wasm_url = plugin.get("wasm_url")
        wasm_sha256 = plugin.get("wasm_sha256")

        if not wasm_url:
            errors.append(f"{plugin_id}: missing wasm_url")
            continue
        if not wasm_sha256:
            errors.append(f"{plugin_id}: missing wasm_sha256")
            continue
        if not wasm_url.startswith(RAW_PREFIX):
            errors.append(f"{plugin_id}: wasm_url must start with {RAW_PREFIX}")
            continue

        artifact_name = wasm_url.removeprefix(RAW_PREFIX)
        artifact_path = DIST_DIR / artifact_name
        if not artifact_path.is_file():
            errors.append(f"{plugin_id}: missing dist artifact {artifact_name}")
            continue

        actual_sha256 = sha256_file(artifact_path)
        if actual_sha256 != wasm_sha256:
            errors.append(
                f"{plugin_id}: sha256 mismatch (registry={wasm_sha256}, actual={actual_sha256})"
            )

    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1

    print("registry OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
