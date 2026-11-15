#!/usr/bin/env bash
# Checks that the working tree is a releasable `<version>`. Run from the
# repository root:
#
#   .github/scripts/check-release-version.sh <version>
#
# It fails unless
#   1. <version> is a semver version (`X.Y.Z` or `X.Y.Z-pre`);
#   2. `ferroly` and `ferroly-derive` both have exactly that version, and
#      ferroly's requirement on ferroly-derive is that version (`^X.Y.Z` or
#      `=X.Y.Z`), so ferroly never resolves an older derive crate;
#   3. CHANGELOG.md has a `## [<version>]` entry (`## [Unreleased]` does not
#      count).
#
# version-guard (on PRs), tag-on-merge (before it creates the tag) and
# publish.yml (before the dry-run) all run this one script, so a version that
# passes CI is one that tags and publishes. publish.yml runs the copy from its
# own commit on main, not the one in the tagged tree.
#
# Needs cargo (for `cargo metadata`) and python3 on PATH.
set -euo pipefail

VERSION="${1:-}"
SEMVER='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'
# `[[ =~ ]]` matches the whole string; `grep` would accept any one line of it.
if ! [[ "$VERSION" =~ $SEMVER ]]; then
  echo "::error::'$VERSION' is not a <major>.<minor>.<patch>[-<pre>] version"
  exit 1
fi

meta="$(mktemp)"
trap 'rm -f "$meta"' EXIT
cargo metadata --no-deps --format-version 1 --locked > "$meta"
python3 -I - "$VERSION" "$meta" <<'PY'
import json, sys

version, path = sys.argv[1], sys.argv[2]
with open(path) as f:
    pkgs = {p["name"]: p for p in json.load(f)["packages"]}
errors = []
for name in ("ferroly-derive", "ferroly"):
    if name not in pkgs:
        errors.append(f"the workspace has no {name} package")
    elif pkgs[name]["version"] != version:
        errors.append(f"{name} is {pkgs[name]['version']}, expected {version}")
if "ferroly" in pkgs:
    req = [d["req"] for d in pkgs["ferroly"]["dependencies"] if d["name"] == "ferroly-derive"]
    if req != [f"^{version}"] and req != [f"={version}"]:
        errors.append(f"ferroly requires ferroly-derive {req}, expected {version}"
                      " (set it in [workspace.dependencies] of the root Cargo.toml)")
for e in errors:
    print(f"::error::{e}")
sys.exit(1 if errors else 0)
PY

ESCAPED="$(printf '%s' "$VERSION" | sed 's/\./\\./g')"
if ! grep -qE "^## \[${ESCAPED}\]([[:space:]]|$)" CHANGELOG.md; then
  echo "::error::CHANGELOG.md has no '## [$VERSION]' entry"
  exit 1
fi

echo "OK: ferroly and ferroly-derive are $VERSION, and CHANGELOG.md documents it."
