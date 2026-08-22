#!/usr/bin/env sh
# Tests for scripts/bump-version.sh. Runs the real script against throwaway fixture
# workspaces in temp dirs, so the repo's own version files are never touched.
set -eu

script="$(cd "$(dirname "$0")/.." && pwd)/scripts/bump-version.sh"
fail=0

# A minimal but real cargo workspace at version $1 in a fresh temp dir; prints its path.
make_fixture() {
  ver="$1"
  d="$(mktemp -d)"
  mkdir -p "$d/crates/demo/src" "$d/packaging/arch"
  cat > "$d/Cargo.toml" <<EOF
[workspace]
members = ["crates/demo"]

[workspace.package]
version = "$ver"
edition = "2021"
EOF
  cat > "$d/crates/demo/Cargo.toml" <<EOF
[package]
name = "demo"
version.workspace = true
edition.workspace = true
EOF
  echo 'pub fn demo() {}' > "$d/crates/demo/src/lib.rs"
  printf 'pkgname=taffle\npkgver=%s\npkgrel=1\n' "$ver" > "$d/packaging/arch/PKGBUILD"
  ( cd "$d" && cargo generate-lockfile >/dev/null 2>&1 )
  echo "$d"
}

check() {
  desc="$1"; got="$2"; want="$3"
  if [ "$got" = "$want" ]; then echo "ok   - $desc"
  else echo "FAIL - $desc (got '$got', want '$want')"; fail=1; fi
}

toml_ver() { sed -n 's/^version = "\(.*\)"$/\1/p' "$1/Cargo.toml"; }
pkgbuild_ver() { sed -n 's/^pkgver=//p' "$1/packaging/arch/PKGBUILD"; }
lock_ver() { sed -n '/^name = "demo"$/{n;s/^version = "\(.*\)"$/\1/p;}' "$1/Cargo.lock"; }

# patch: 0.1.0 -> 0.1.1, and every file agrees
d="$(make_fixture 0.1.0)"
out="$( cd "$d" && sh "$script" patch )"
check "patch prints new version" "$out" "0.1.1"
check "patch Cargo.toml"  "$(toml_ver "$d")"     "0.1.1"
check "patch PKGBUILD"    "$(pkgbuild_ver "$d")" "0.1.1"
check "patch Cargo.lock"  "$(lock_ver "$d")"     "0.1.1"
rm -rf "$d"

# minor: 0.1.5 -> 0.2.0 (patch resets to 0)
d="$(make_fixture 0.1.5)"
out="$( cd "$d" && sh "$script" minor )"
check "minor prints new version" "$out" "0.2.0"
check "minor resets patch"       "$(toml_ver "$d")" "0.2.0"
rm -rf "$d"

# major: 1.2.3 -> 2.0.0 (minor and patch reset to 0)
d="$(make_fixture 1.2.3)"
out="$( cd "$d" && sh "$script" major )"
check "major prints new version" "$out" "2.0.0"
check "major resets minor+patch" "$(toml_ver "$d")" "2.0.0"
rm -rf "$d"

# BUMP_FLOOR above the committed version bumps from the floor, and the result is
# written over the committed version in every file: committed 2.0.0, floor 3.0.0,
# major -> 4.0.0.
d="$(make_fixture 2.0.0)"
out="$( cd "$d" && BUMP_FLOOR=3.0.0 sh "$script" major )"
check "floor above current: prints" "$out" "4.0.0"
check "floor above current: Cargo.toml" "$(toml_ver "$d")"     "4.0.0"
check "floor above current: PKGBUILD"   "$(pkgbuild_ver "$d")" "4.0.0"
check "floor above current: Cargo.lock" "$(lock_ver "$d")"     "4.0.0"
rm -rf "$d"

# A floor at or below the committed version is ignored: bump straight from current.
d="$(make_fixture 2.0.0)"
out="$( cd "$d" && BUMP_FLOOR=1.5.0 sh "$script" patch )"
check "floor below current ignored" "$out" "2.0.1"
rm -rf "$d"

# A malformed floor exits non-zero.
d="$(make_fixture 2.0.0)"
if ( cd "$d" && BUMP_FLOOR=nope sh "$script" patch >/dev/null 2>&1 ); then
  echo "FAIL - malformed BUMP_FLOOR should exit non-zero"; fail=1
else echo "ok   - malformed BUMP_FLOOR rejected"; fi
rm -rf "$d"

# A floor with a non-numeric component exits non-zero and touches nothing: this is the
# glob-vs-regex boundary a naive `case` pattern lets through (leading digit satisfies
# `[0-9]*`, and `*` after the dot absorbs the rest of the component unchecked).
d="$(make_fixture 1.0.0)"
if ( cd "$d" && BUMP_FLOOR=9.9x.9 sh "$script" patch >/dev/null 2>&1 ); then
  echo "FAIL - non-numeric BUMP_FLOOR component should exit non-zero"; fail=1
else echo "ok   - non-numeric BUMP_FLOOR component rejected"; fi
check "non-numeric floor: Cargo.toml untouched" "$(toml_ver "$d")" "1.0.0"
rm -rf "$d"

# bad argument exits non-zero
d="$(make_fixture 0.1.0)"
if ( cd "$d" && sh "$script" bogus >/dev/null 2>&1 ); then
  echo "FAIL - bad argument should exit non-zero"; fail=1
else echo "ok   - bad argument rejected"; fi
rm -rf "$d"

# malformed current version exits non-zero
d="$(make_fixture 0.1.0)"
sed -i 's/^version = ".*"$/version = "not-a-version"/' "$d/Cargo.toml"
if ( cd "$d" && sh "$script" patch >/dev/null 2>&1 ); then
  echo "FAIL - malformed current version should exit non-zero"; fail=1
else echo "ok   - malformed current version rejected"; fi
rm -rf "$d"

exit $fail
