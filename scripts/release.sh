#!/usr/bin/env bash
#
# release.sh — build, version-bump, and release a WASM plugin
#
# Builds the WASM binary, updates registry.json, commits, tags, and pushes.
#
# Usage:
#   ./scripts/release.sh animetosho              # auto-increment patch
#   ./scripts/release.sh animetosho --minor      # increment minor
#   ./scripts/release.sh animetosho --major      # increment major
#   ./scripts/release.sh animetosho 0.2.0        # explicit version
#   ./scripts/release.sh animetosho --dry-run    # validate only
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"
REGISTRY="$REPO_ROOT/registry.json"
VALIDATE_REGISTRY="$REPO_ROOT/scripts/validate_registry.py"

# ── Colors ────────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; RESET='\033[0m'

step() { echo -e "\n${BLUE}${BOLD}▶  $*${RESET}"; }
ok()   { echo -e "   ${GREEN}✓  $*${RESET}"; }
warn() { echo -e "   ${YELLOW}⚠  $*${RESET}"; }
die()  { echo -e "\n${RED}${BOLD}✗  $*${RESET}" >&2; exit 1; }

# ── Argument parsing ──────────────────────────────────────────────────────────
PLUGIN_NAME=""
BUMP="patch"
EXPLICIT_VERSION=""
DRY_RUN=false

for arg in "$@"; do
    case "$arg" in
        --major)   BUMP="major" ;;
        --minor)   BUMP="minor" ;;
        --patch)   BUMP="patch" ;;
        --dry-run) DRY_RUN=true ;;
        v[0-9]*.[0-9]*.[0-9]*) EXPLICIT_VERSION="${arg#v}" ;;
        [0-9]*.[0-9]*.[0-9]*)  EXPLICIT_VERSION="$arg" ;;
        -*)        die "Unknown flag: $arg" ;;
        *)
            if [[ -z "$PLUGIN_NAME" ]]; then
                PLUGIN_NAME="$arg"
            else
                die "Unexpected argument: $arg"
            fi
            ;;
    esac
done

[[ -n "$PLUGIN_NAME" ]] || die "Usage: $0 <plugin-name> [--patch|--minor|--major|VERSION] [--dry-run]"

# ── Locate plugin ────────────────────────────────────────────────────────────
# Search all plugin type directories for the given name
PLUGIN_DIR=""
for type_dir in "$REPO_ROOT"/indexers "$REPO_ROOT"/download_clients "$REPO_ROOT"/notifications; do
    if [[ -d "$type_dir/$PLUGIN_NAME" ]]; then
        PLUGIN_DIR="$type_dir/$PLUGIN_NAME"
        break
    fi
done

[[ -n "$PLUGIN_DIR" ]] || die "Plugin '$PLUGIN_NAME' not found in any plugin directory"

CARGO_TOML="$PLUGIN_DIR/Cargo.toml"
[[ -f "$CARGO_TOML" ]] || die "No Cargo.toml at $CARGO_TOML"

# Read the crate name (for the WASM filename)
CRATE_NAME="$(grep -m1 '^name = ' "$CARGO_TOML" | sed 's/.*"\(.*\)".*/\1/')"
WASM_FILENAME="${CRATE_NAME//-/_}.wasm"

echo "   Plugin dir : $PLUGIN_DIR"
echo "   Crate name : $CRATE_NAME"
echo "   WASM file  : $WASM_FILENAME"

# ── Determine version ─────────────────────────────────────────────────────────
step "Determining next version"

CURRENT_VERSION="$(grep -m1 '^version = ' "$CARGO_TOML" | sed 's/.*"\(.*\)".*/\1/')"

if [[ -n "$EXPLICIT_VERSION" ]]; then
    NEXT_VERSION="$EXPLICIT_VERSION"
else
    IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_VERSION"
    case "$BUMP" in
        major) NEXT_VERSION="$((MAJOR + 1)).0.0" ;;
        minor) NEXT_VERSION="${MAJOR}.$((MINOR + 1)).0" ;;
        patch) NEXT_VERSION="${MAJOR}.${MINOR}.$((PATCH + 1))" ;;
    esac
fi

TAG_NAME="${PLUGIN_NAME}-v${NEXT_VERSION}"

echo "   Current    : $CURRENT_VERSION"
echo "   Next       : $NEXT_VERSION"
echo "   Tag        : $TAG_NAME"
$DRY_RUN && echo -e "   ${YELLOW}(dry run — no commits, tags, or pushes)${RESET}"

# ── Pre-flight checks ────────────────────────────────────────────────────────
step "Pre-flight checks"

cd "$REPO_ROOT"

if git tag | grep -qx "$TAG_NAME"; then
    die "Tag $TAG_NAME already exists"
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
echo "   Branch: $BRANCH"

# Check the plugin is in registry.json
if ! python3 -c "
import json, sys
with open('$REGISTRY') as f:
    reg = json.load(f)
found = any(p['id'] == '$PLUGIN_NAME' for p in reg['plugins'])
sys.exit(0 if found else 1)
" 2>/dev/null; then
    die "Plugin '$PLUGIN_NAME' not found in registry.json"
fi

# Verify it's not a builtin (builtins don't get distributed as WASM)
IS_BUILTIN="$(python3 -c "
import json
with open('$REGISTRY') as f:
    reg = json.load(f)
p = next(p for p in reg['plugins'] if p['id'] == '$PLUGIN_NAME')
print('true' if p.get('builtin', False) else 'false')
")"

if [[ "$IS_BUILTIN" == "true" ]]; then
    die "Plugin '$PLUGIN_NAME' is builtin — builtin plugins are released with scryer, not independently"
fi

# Check for wasm32-wasip1 target
if ! rustup target list --installed | grep -q wasm32-wasip1; then
    die "wasm32-wasip1 target not installed — run: rustup target add wasm32-wasip1"
fi

ok "Pre-flight OK"

# ── Bump version in Cargo.toml ───────────────────────────────────────────────
step "Bumping $CRATE_NAME to $NEXT_VERSION"

sed -i '' 's/^version = "[^"]*"/version = "'"$NEXT_VERSION"'"/' "$CARGO_TOML"

WRITTEN="$(grep -m1 '^version = ' "$CARGO_TOML" | sed 's/.*"\(.*\)".*/\1/')"
[[ "$WRITTEN" == "$NEXT_VERSION" ]] || die "Version write failed — Cargo.toml shows: $WRITTEN"

ok "Cargo.toml updated"

# ── Build WASM ────────────────────────────────────────────────────────────────
step "Building WASM (release, wasm32-wasip1)"

cd "$PLUGIN_DIR"
cargo build --release --target wasm32-wasip1 2>&1 || die "WASM build failed"

BUILT_WASM="$PLUGIN_DIR/target/wasm32-wasip1/release/$WASM_FILENAME"
[[ -f "$BUILT_WASM" ]] || die "Expected WASM at $BUILT_WASM but not found"

ok "Built $WASM_FILENAME"

# ── Copy to dist and compute SHA256 ──────────────────────────────────────────
step "Updating dist/$WASM_FILENAME"

mkdir -p "$DIST_DIR"
cp "$BUILT_WASM" "$DIST_DIR/$WASM_FILENAME"

SHA256="$(shasum -a 256 "$DIST_DIR/$WASM_FILENAME" | awk '{print $1}')"

echo "   SHA256: $SHA256"
ok "Copied to dist/"

# ── Update registry.json ─────────────────────────────────────────────────────
step "Updating registry.json"

python3 -c "
import json

with open('$REGISTRY') as f:
    reg = json.load(f)

for p in reg['plugins']:
    if p['id'] == '$PLUGIN_NAME':
        p['version'] = '$NEXT_VERSION'
        p['wasm_url'] = 'https://raw.githubusercontent.com/scryer-media/scryer-plugins/main/dist/$WASM_FILENAME'
        p['wasm_sha256'] = '$SHA256'
        break

with open('$REGISTRY', 'w') as f:
    json.dump(reg, f, indent=2)
    f.write('\n')
"

ok "registry.json updated (version=$NEXT_VERSION, sha256=$SHA256)"

# ── Validate registry consistency ────────────────────────────────────────────
step "Validating registry"

python3 "$VALIDATE_REGISTRY" || die "Registry validation failed"

ok "Registry validation passed"

# ── Dry-run exit ──────────────────────────────────────────────────────────────
if $DRY_RUN; then
    echo ""
    echo -e "${YELLOW}${BOLD}Dry run complete — stopping before commit/tag/push.${RESET}"
    echo -e "  $PLUGIN_NAME $NEXT_VERSION validated OK."
    # Restore changes
    cd "$REPO_ROOT"
    git checkout -- "$CARGO_TOML" "$REGISTRY"
    git checkout -- "$DIST_DIR/$WASM_FILENAME" 2>/dev/null || true
    exit 0
fi

# ── Commit ────────────────────────────────────────────────────────────────────
step "Committing changes"

cd "$REPO_ROOT"

git add "$CARGO_TOML" "$REGISTRY" "$DIST_DIR/$WASM_FILENAME"
git commit -m "release: $PLUGIN_NAME $NEXT_VERSION"

ok "Committed"

# ── Create signed tag ────────────────────────────────────────────────────────
step "Creating signed tag $TAG_NAME"

git tag -s "$TAG_NAME" -m "Release $TAG_NAME"
ok "Tag $TAG_NAME created"

# ── Push ──────────────────────────────────────────────────────────────────────
step "Pushing to origin"

git push origin "$BRANCH"
git push origin "$TAG_NAME"
ok "Pushed $BRANCH and tag $TAG_NAME"

# ── Done ──────────────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}Released $TAG_NAME${RESET}"
