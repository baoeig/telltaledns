#!/usr/bin/env bash
# REQ: T8.2, API-002 (ADR-171) — a router added through the API, as the UI does: its API key is
# saved to a private file under the data directory (never stored, listed, or audited), the
# router is read without a restart, a change to another field keeps the key, a pasted key
# can't be written outside the secrets directory, and a delete removes the router and its key.
# Needs python3, curl.
# Usage: deploy/routers-ui-e2e.sh [path/to/telltale]   (default: target/debug/telltale)
set -euo pipefail
cd "$(dirname "$0")/.."
B=${1:-target/debug/telltale}
B=$(cd "$(dirname "$B")" && pwd)/$(basename "$B")
E=$(mktemp -d)
P=
H=
cleanup() {
  local rc=$?
  for p in $P $H; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$E"
  exit "$rc"
}
trap cleanup EXIT
fail() {
  echo "FAIL: $*"
  [ -n "${GITHUB_ACTIONS:-}" ] && echo "::error title=$(basename "$0")::FAIL: $*"
  [ -f "$E/node.log" ] && tail -15 "$E/node.log"
  exit 1
}
trap 'fail "line $LINENO: \`$BASH_COMMAND\` failed"' ERR
API=http://127.0.0.1:26993
KEY=ui-pasted-key-0123456789
field() { python3 -c "import sys,json; d=json.load(sys.stdin); print($1)"; }

cat > "$E/fake.py" <<'EOF'
import http.server, json, sys
KEY = sys.argv[1]
class UniFi(http.server.BaseHTTPRequestHandler):
    def reply(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        if self.headers.get("x-api-key") != KEY:
            return self.reply(401, {"meta": {"rc": "error"}})
        if self.path == "/proxy/network/api/s/default/stat/sta":
            return self.reply(200, {"meta": {"rc": "ok"}, "data": [
                {"mac": "02:00:00:00:01:01", "ip": "192.168.7.20", "name": "Hall tablet"}]})
        self.reply(404, {})
    def log_message(self, *a):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", 26984), UniFi).serve_forever()
EOF
python3 "$E/fake.py" "$KEY" & H=$!
for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:26984/ && break; sleep 0.1; done

cat > "$E/telltale.toml" <<EOF
[node]
data_dir = "$E/data"
[[listen]]
proto = "udp"
addr = "127.0.0.1:25993"
[api]
listen = "127.0.0.1:26993"
[telemetry.metrics]
listen = "127.0.0.1:27993"
EOF
"$B" run -c "$E/telltale.toml" > "$E/node.log" 2>&1 & P=$!
for _ in $(seq 100); do curl -s -o /dev/null "$API/api/v1/auth/status" && break; sleep 0.1; done
ST=$(cat "$E/data/setup-token")
CSRF=$(curl -sf -c "$E/jar" -H 'content-type: application/json' \
  -d "{\"setupToken\":\"$ST\",\"username\":\"admin\",\"password\":\"routers-ui-e2e-pass-1\"}" \
  "$API/api/v1/auth/setup" | field 'd["csrfToken"]')
put() { curl -s -o "$E/out" -w '%{http_code}' -X PUT -b "$E/jar" -H "x-csrf-token: $CSRF" -H 'content-type: application/json' -d "$2" "$API/api/v1/routers/$1"; }

# A pasted key outside the secrets directory is refused, and nothing is written.
code=$(put udm "{\"type\":\"unifi\",\"url\":\"http://127.0.0.1:26984\",\"api_key\":\"$KEY\",\"api_key_file\":\"$E/elsewhere\"}")
[ "$code" = 422 ] || fail "a key outside secrets/ wasn't refused ($code): $(cat "$E/out")"
[ ! -e "$E/elsewhere" ] || fail "the key was written outside secrets/"

# Added with a pasted key: saved to the default file, 0600, and read without a restart.
code=$(put udm "{\"type\":\"unifi\",\"url\":\"http://127.0.0.1:26984\",\"api_key\":\"$KEY\",\"offline_days\":0}")
[ "$code" = 200 ] || fail "PUT /routers/udm: $code $(cat "$E/out")"
F="$E/data/secrets/router-udm.api_key"
[ "$(cat "$F")" = "$KEY" ] || fail "the key file"
[ "$(stat -c %a "$F")" = 600 ] || fail "the key file is $(stat -c %a "$F"), not 600"
[ "$(stat -c %a "$E/data/secrets")" = 700 ] || fail "the secrets directory isn't 700"
for _ in $(seq 50); do grep -q 'router="udm".*devices read\|router=udm.*devices read' "$E/node.log" && break; sleep 0.2; done
curl -sf -b "$E/jar" "$API/api/v1/dhcp/leases" | python3 -c '
import json, sys
items = {x["ip"]: x for x in json.load(sys.stdin)["items"]}
assert items["192.168.7.20"]["hostname"] == "Hall tablet", items
' || fail "the router added through the API wasn't read"
echo "added: key saved to secrets/router-udm.api_key (0600), router read without a restart"

# The key is nowhere the API can show it.
curl -sf -b "$E/jar" "$API/api/v1/config/entries?kind=router" > "$E/entries"
grep -q "router-udm.api_key" "$E/entries" || fail "the entry doesn't name its key file: $(cat "$E/entries")"
curl -sf -b "$E/jar" "$API/api/v1/audit" > "$E/audit"
! grep -q "$KEY" "$E/entries" "$E/audit" "$E/node.log" || fail "the key shows up in the API or the log"

# Changing another field keeps the key.
code=$(put udm '{"type":"unifi","url":"http://127.0.0.1:26984","interval_secs":60,"offline_days":0}')
[ "$code" = 200 ] || fail "PUT without the key: $code $(cat "$E/out")"
curl -sf -b "$E/jar" "$API/api/v1/config/entries?kind=router" | grep -q "router-udm.api_key" || fail "the key file was dropped"
echo "changed: the key is kept, and never shown"

# Deleting it removes the router, its names, and its key file.
code=$(curl -s -o "$E/out" -w '%{http_code}' -X DELETE -b "$E/jar" -H "x-csrf-token: $CSRF" "$API/api/v1/routers/udm")
[ "$code" = 200 ] || fail "DELETE: $code $(cat "$E/out")"
[ ! -e "$F" ] || fail "the key file is still there"
for _ in $(seq 50); do
  curl -sf -b "$E/jar" "$API/api/v1/dhcp/leases" | grep -q 192.168.7.20 || break
  sleep 0.2
done
! curl -sf -b "$E/jar" "$API/api/v1/dhcp/leases" | grep -q 192.168.7.20 || fail "the deleted router's names are still listed"
echo "deleted: router, names, and key file gone"
echo "routers-ui-e2e: ok"
