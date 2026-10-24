#!/bin/sh
# Packaging test: `cargo package --workspace`, unpack both .crate files in a fresh
# directory, build a new project that depends on the packaged datumesh-store (and
# datumesh-core 0.3.3 from crates.io) and run the README example against PostgreSQL 16 in a
# database of its own (an empty one: the example makes the schema through the store, and
# names no `postgres` type); build the packaged command (with the packaged library patched
# in) and run `migrate --check` there. The packaged library must carry no test material.
#
#   tools/package_test.sh [toolchain]   in containers on the `datumesh` network, against the
#                                       datumesh-pg container (see tools/cargo.sh)
#   tools/package_test.sh --native      with the local cargo and DATABASE_URL (CI)
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/crates/datumesh-store/Cargo.toml" | head -n 1)"
work="$(mktemp -d)"
db="dm_package_$$"

if [ "$1" = "--native" ]; then
  (cd "$root" && cargo package --workspace --locked)
  pkg="$root/target/package"
  admin="$DATABASE_URL"
  in_fresh() { (cd "$work" && sh -c "$1"); }
  psql_admin() { psql "$admin" -qc "$1"; }
else
  tag="${1:-1.99}"
  "$root/tools/cargo.sh" "$tag" package --workspace --locked
  pkg="$root/target-$tag/package"
  admin="postgres://postgres:postgres@datumesh-pg:5432/postgres"
  in_fresh() {
    docker run --rm --network datumesh -v "$work:/p" -w /p -e DATABASE_URL="$DATABASE_URL" "rust:$tag" sh -c "$1"
  }
  psql_admin() { docker exec datumesh-pg psql -U postgres -qc "$1"; }
fi

cp "$pkg/datumesh-store-$version.crate" "$pkg/datumesh-store-cli-$version.crate" "$work/"
(cd "$work" && tar xzf "datumesh-store-$version.crate" && tar xzf "datumesh-store-cli-$version.crate")
mkdir -p "$work/app/src" "$work/datumesh-store-cli-$version/.cargo"
cp "$root/spec/vectors/receipt/stored-number.json" "$work/app/"
# The README's first ```rust block is the example.
awk '/^```rust$/{f=1;next} /^```$/{if(f){exit}} f' "$root/README.md" > "$work/app/src/main.rs"
if grep -n 'postgres' "$work/app/src/main.rs"; then
  echo "the README example names postgres" >&2
  exit 1
fi
if tar tzf "$work/datumesh-store-$version.crate" | grep -E '\.(pem|crt|key)$|/tests/'; then
  echo "test material in the packaged library" >&2
  exit 1
fi
printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\ndatumesh-store = { path = "../datumesh-store-%s" }\ndatumesh-core = "0.3.3"\n' "$version" > "$work/app/Cargo.toml"
printf '[patch.crates-io]\ndatumesh-store = { path = "../datumesh-store-%s" }\n' "$version" > "$work/datumesh-store-cli-$version/.cargo/config.toml"

psql_admin "CREATE DATABASE $db"
trap 'psql_admin "DROP DATABASE IF EXISTS $db WITH (FORCE)"; rm -rf "$work"' EXIT
DATABASE_URL="${admin%/*}/$db"
export DATABASE_URL
# Twice: the example is idempotent.
in_fresh "cd app && cargo run -q && cargo run -q"
in_fresh "cd datumesh-store-cli-$version && cargo run -q -- migrate --check"
echo "package test passed ($version)"
