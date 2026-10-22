#!/bin/sh
# The library's public API (`cargo public-api -ss`, which needs a nightly toolchain and
# cargo-public-api 0.52): with default features and with all features (the crate has none,
# so the two must be equal), checked to name no type of the PostgreSQL driver or of a TLS
# crate outside `datumesh_store::pg_unstable`, and compared with public-api.txt.
#
#   tools/public_api.sh [--write]            in the datumesh-nightly-tools image (tools/cargo.sh)
#   tools/public_api.sh --native [--write]   with the local cargo (CI)
# --write updates public-api.txt instead of failing when it differs.
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
native=false
write=false
for a in "$@"; do
  case "$a" in --native) native=true ;; --write) write=true ;; *) echo "unknown option $a" >&2; exit 2 ;; esac
done
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

api() {
  if $native; then
    (cd "$root" && cargo public-api -p datumesh-store -ss "$@")
  else
    "$root/tools/cargo.sh" nightly-tools public-api -p datumesh-store -ss "$@"
  fi
}
api > "$work/default.txt"
api --all-features > "$work/all.txt"
diff -u "$work/default.txt" "$work/all.txt"

# Identifiers of the driver and TLS crates, on any line not about pg_unstable.
leaks="$(grep -v 'datumesh_store::pg_unstable' "$work/default.txt" \
  | grep -E '(^|[^A-Za-z0-9_])(postgres|tokio_postgres|postgres_types|postgres_protocol|rustls|tokio_rustls|tokio_postgres_rustls|rustls_pki_types|pki_types|webpki|webpki_roots|ring)([^A-Za-z0-9_]|$)' || true)"
if [ -n "$leaks" ]; then
  echo "driver or TLS types outside pg_unstable:" >&2
  echo "$leaks" >&2
  exit 1
fi
echo "no driver or TLS types outside datumesh_store::pg_unstable"

if $write; then
  cp "$work/default.txt" "$root/public-api.txt"
  echo "public-api.txt written"
else
  diff -u "$root/public-api.txt" "$work/default.txt"
  echo "public-api.txt is current"
fi
