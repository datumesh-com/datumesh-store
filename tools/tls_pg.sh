#!/bin/sh
# A PostgreSQL 16 server with TLS, for the TLS tests (crates/datumesh-store/tests/tls.rs and
# the command's). Generates a throwaway CA and a server certificate for the host names
# datumesh-tls-pg and localhost (openssl in the postgres:16 image), written to DIR and never
# committed: test-only material, made anew on every run. Then (re)starts the container
# datumesh-tls-pg with ssl=on on the `datumesh` Docker network; plain-text connections stay
# allowed (sslmode=disable is tested too). With PORT, it is also published on 127.0.0.1:PORT.
#
# Usage: tools/tls_pg.sh [DIR] [PORT]
#   DIR defaults to target-tls in the repository (ignored by git).
# Then, on the `datumesh` network (tools/cargo.sh passes these through):
#   DATABASE_TLS_URL=postgres://postgres:postgres@datumesh-tls-pg:5432/postgres
#   DATABASE_TLS_CA_FILE=/w/target-tls/ca.crt
# or, with PORT, from the host: postgres://postgres:postgres@localhost:PORT/postgres and DIR/ca.crt.
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
dir="${1:-$root/target-tls}"
port="$2"
name="${TLS_PG_NAME:-datumesh-tls-pg}"

mkdir -p "$dir"
dir="$(cd "$dir" && pwd)"
docker network inspect datumesh >/dev/null 2>&1 || docker network create datumesh >/dev/null

# The CA (P-256, 30 days) and a server certificate it issues, valid for the container's name
# on the network and for localhost; no IP addresses, so connecting by IP is a wrong host name.
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -v "$dir:/t" -w /t postgres:16 sh -ec '
  rm -f ca.key ca.crt ca.srl server.key server.csr server.crt server.ext
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 30 \
    -subj "/CN=datumesh-store test CA (throwaway)" -keyout ca.key -out ca.crt \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=datumesh-tls-pg" \
    -keyout server.key -out server.csr 2>/dev/null
  printf "subjectAltName=DNS:datumesh-tls-pg,DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n" > server.ext
  openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext \
    -out server.crt 2>/dev/null
  chmod 644 ca.crt server.crt
'

docker rm -f "$name" >/dev/null 2>&1 || true
publish=""
[ -n "$port" ] && publish="-p 127.0.0.1:$port:5432"
# The key must belong to the server's user and not be readable by others.
# shellcheck disable=SC2086
docker run -d --name "$name" --network datumesh $publish -v "$dir:/tls:ro" -e POSTGRES_PASSWORD=postgres \
  postgres:16 sh -ec '
    install -o postgres -g postgres -m 600 /tls/server.key /var/lib/postgresql/server.key
    install -o postgres -g postgres -m 644 /tls/server.crt /var/lib/postgresql/server.crt
    exec docker-entrypoint.sh postgres -c ssl=on \
      -c ssl_cert_file=/var/lib/postgresql/server.crt -c ssl_key_file=/var/lib/postgresql/server.key
  ' >/dev/null
# Ready when it answers over TCP (the initialisation server listens on the socket only).
i=0
until docker exec -e PGPASSWORD=postgres "$name" psql -h 127.0.0.1 -U postgres -Atqc "SELECT 1" >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -gt 120 ] && { docker logs "$name"; echo "$name did not start" >&2; exit 1; }
  sleep 1
done
docker exec -e PGPASSWORD=postgres "$name" psql -h 127.0.0.1 -U postgres -Atqc "SHOW ssl" | grep -qx on
echo "$name: TLS on; CA $dir/ca.crt"
