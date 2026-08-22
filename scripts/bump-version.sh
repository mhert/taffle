#!/usr/bin/env sh
# Advance the workspace version (Semantic Versioning) in every place it is committed:
#   Cargo.toml  [workspace.package] version  — the source of truth (all crates inherit it)
#   Cargo.lock  workspace-member entries      — synced with `cargo update --workspace`
#   packaging/arch/PKGBUILD  pkgver=          — the only packaging file with a committed version
#
# Windows (installer.nsi) and Debian take their version from the build command
# (-DVERSION / cargo deb --deb-version), so they have nothing committed to bump.
#
# Usage: scripts/bump-version.sh <patch|minor|major>
# Prints the new X.Y.Z on stdout. Acts on files relative to the current directory;
# committing and tagging are the caller's job.
#
# Set BUMP_FLOOR=X.Y.Z to bump from max(committed version, floor) instead of the
# committed version. The release workflow passes the highest published tag as the
# floor, so a release is computed from the newest published version even when the
# committed version has drifted behind it (e.g. after a history rewrite). The new
# version is written over whatever version the files currently hold, so they need
# not already contain the floor.
set -eu

bump="${1:-}"
case "$bump" in
  patch|minor|major) ;;
  *) echo "usage: $0 <patch|minor|major>" >&2; exit 2 ;;
esac

# The first top-level `version = "X.Y.Z"` line is the [workspace.package] value; dotted
# keys like `version.workspace = true` and inline dependency specs never match `^version = "`.
cur="$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -n1)"
if [ -z "$cur" ]; then
  echo "error: no [workspace.package] version = \"X.Y.Z\" found in Cargo.toml" >&2
  exit 1
fi

# Bump from the floor when it is higher than the committed version; otherwise from
# the committed version. `new` is applied over the files' current version below, so
# the files need not already hold the floor. Validated strictly: each component must
# be all digits with no leading zero (a bare 0 is fine), because a loose check here
# lets a non-numeric component (e.g. "9x") through as `base`, which later fails the
# arithmetic bump only after the version has already been written into Cargo.toml and
# PKGBUILD, corrupting both.
base="$cur"
if [ -n "${BUMP_FLOOR:-}" ]; then
  if ! printf '%s' "$BUMP_FLOOR" | grep -qE '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'; then
    echo "error: BUMP_FLOOR is not X.Y.Z: '$BUMP_FLOOR'" >&2; exit 1
  fi
  base="$(printf '%s\n%s\n' "$cur" "$BUMP_FLOOR" | sort -V | tail -n1)"
fi

major="${base%%.*}"; rest="${base#*.}"; minor="${rest%%.*}"; patch="${rest#*.}"
case "$bump" in
  major) major=$((major + 1)); minor=0; patch=0 ;;
  minor) minor=$((minor + 1)); patch=0 ;;
  patch) patch=$((patch + 1)) ;;
esac
new="${major}.${minor}.${patch}"

sed -i "s/^version = \"${cur}\"\$/version = \"${new}\"/" Cargo.toml
sed -i "s/^pkgver=.*/pkgver=${new}/" packaging/arch/PKGBUILD
# Rewrite the workspace-member versions in Cargo.lock without re-resolving dependencies.
# cargo writes progress to stderr; redirect its stdout there too so ours stays clean.
cargo update --workspace 1>&2

echo "$new"
