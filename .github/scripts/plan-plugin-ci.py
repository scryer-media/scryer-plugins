#!/usr/bin/env python3
"""Classify a change set for the plugin CI workflow."""

from __future__ import annotations

import argparse
import json
import tomllib
from pathlib import Path, PurePosixPath


def official_plugins(root: Path) -> dict[str, PurePosixPath]:
    plugins: dict[str, PurePosixPath] = {}
    for manifest in sorted(root.glob("*/*/Cargo.toml")):
        with manifest.open("rb") as source:
            document = tomllib.load(source)
        metadata = document.get("package", {}).get("metadata", {}).get("scryer", {})
        if metadata.get("official") is not True:
            continue
        plugin_id = metadata.get("plugin_id")
        if not isinstance(plugin_id, str) or not plugin_id:
            raise ValueError(f"official plugin is missing plugin_id: {manifest}")
        plugin_dir = PurePosixPath(manifest.parent.relative_to(root).as_posix())
        if plugin_id in plugins:
            raise ValueError(f"duplicate official plugin_id: {plugin_id}")
        plugins[plugin_id] = plugin_dir
    return plugins


def documentation_only(path: PurePosixPath) -> bool:
    return path.suffix.lower() in {".md", ".mdx"} or path.name in {"LICENSE", "NOTICE"}


def rule_pack_data(path: PurePosixPath) -> bool:
    # Rule packs are published data; no plugin builds or tests read them.
    return path.parts[:1] == ("rule_packs",)


def scope_for_changes(
    changed_paths: list[str], plugins: dict[str, PurePosixPath]
) -> tuple[str, list[str]]:
    selected: set[str] = set()
    for raw_path in changed_paths:
        path = PurePosixPath(raw_path)
        if path.is_absolute() or ".." in path.parts:
            return "full", []
        if documentation_only(path) or rule_pack_data(path):
            continue
        for plugin_id, plugin_dir in plugins.items():
            if path == plugin_dir or plugin_dir in path.parents:
                selected.add(plugin_id)
                break
        else:
            return "full", []
    if selected:
        return "scoped", sorted(selected)
    return "none", []


def plugin_matrix(
    mode: str, plugin_ids: list[str], plugins: dict[str, PurePosixPath]
) -> str:
    if mode == "full":
        selected = sorted(plugins)
    elif mode == "scoped":
        selected = plugin_ids
    else:
        selected = []

    entries = [{"plugin_id": plugin_id} for plugin_id in selected]
    if not entries:
        entries = [{"plugin_id": ""}]
    return json.dumps({"include": entries}, separators=(",", ":"))


def quality_matrix(
    mode: str, plugin_ids: list[str], plugins: dict[str, PurePosixPath]
) -> str:
    if mode == "full":
        selected = sorted(plugins)
    elif mode == "scoped":
        selected = plugin_ids
    else:
        selected = []

    entries = [
        {"kind": "xtask", "id": "xtask", "directory": "xtask"},
        *[
            {"kind": "plugin", "id": plugin_id, "directory": str(plugins[plugin_id])}
            for plugin_id in selected
        ],
    ]
    if mode == "none":
        entries = [{"kind": "not-needed", "id": "", "directory": ""}]
    return json.dumps({"include": entries}, separators=(",", ":"))


def main() -> None:
    parser = argparse.ArgumentParser()
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--changed-files", type=Path)
    source.add_argument("--full", action="store_true")
    args = parser.parse_args()

    root = Path.cwd()
    plugins = official_plugins(root)
    if args.full:
        mode, plugin_ids = "full", []
    else:
        assert args.changed_files is not None
        changed_paths = [
            line.strip()
            for line in args.changed_files.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        mode, plugin_ids = scope_for_changes(changed_paths, plugins)
    print(f"mode={mode}")
    print(f"plugin_ids={','.join(plugin_ids)}")
    print(f"plugin_matrix={plugin_matrix(mode, plugin_ids, plugins)}")
    print(f"quality_matrix={quality_matrix(mode, plugin_ids, plugins)}")


if __name__ == "__main__":
    main()
