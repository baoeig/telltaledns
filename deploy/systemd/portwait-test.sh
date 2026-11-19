#!/usr/bin/env bash
# REQ: OPS-004 — install.sh waits (bounded) for port 53 to be free after it turns systemd-resolved's
# stub listener off, says what holds the port when it isn't, and starts the service once.
# No root, nothing is installed and nothing on the machine changes: the wait functions are cut out
# of install.sh (the `port53-wait` block) and run under `sh` against a fake `ss` and a fake
# `telltale`, so every case is deterministic and the sockets of the machine don't matter.
# Usage: deploy/systemd/portwait-test.sh [path/to/telltale]
#   With the real binary it also checks the effective-config format that install.sh reads to know
#   which addresses TelltaleDNS will bind (`telltale config check FILE --no-env --print`).
set -euo pipefail
cd "$(dirname "$0")/../.."
INSTALL=deploy/systemd/install.sh
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
fail() {
  echo "FAIL: $*"
  [ -n "${GITHUB_ACTIONS:-}" ] && echo "::error title=portwait-test::FAIL: $*"
  exit 1
}
n=0
ok() { n=$((n + 1)); echo "ok $1"; }

SH=${PORTWAIT_TEST_SH:-$(command -v sh)}   # the shell that runs install.sh's functions (try busybox ash: PORTWAIT_TEST_SH=/path/to/sh)
FAKE="$T/fake"
FAKEBIN="$T/fakebin"
FAKEROOT="$T/root"
mkdir -p "$FAKE" "$FAKEBIN" "$FAKEROOT/bin"

# ---- the fakes ---------------------------------------------------------------------------------
# ss: a "check" is one `ss -Hlun` plus one `ss -Hltn`. The first $FAKE/busy_checks checks find the
# port held ("always": every check); later ones find it free. `ss -Hlunpt` is the report with
# process names. $FAKE/ss_fails makes it refuse like an old `ss` that doesn't know -H.
cat > "$FAKEBIN/ss" <<'EOF'
#!/bin/sh
case "$*" in *-Hlunpt*) cat "$FAKE/who"; exit 0 ;; esac
if [ -f "$FAKE/ss_fails" ]; then echo "ss: invalid option -- 'H'" >&2; exit 1; fi
case "$*" in
  *-Hlun*) c=$(cat "$FAKE/checks" 2>/dev/null || echo 0); c=$((c + 1)); echo "$c" > "$FAKE/checks"; held=udp_holder ;;
  *-Hltn*) c=$(cat "$FAKE/checks"); held=tcp_holder ;;
  *) exit 1 ;;
esac
busy=$(cat "$FAKE/busy_checks")
if [ "$busy" = always ] || [ "$c" -le "$busy" ]; then cat "$FAKE/$held"; fi
exit 0
EOF
# telltale: only `config check FILE --no-env --print`, which prints the canned effective config.
cat > "$FAKEROOT/bin/telltale" <<'EOF'
#!/bin/sh
[ "$1 $2" = "config check" ] || exit 2
[ "$4 $5" = "--no-env --print" ] || exit 2
[ -f "$FAKE/config_fails" ] && exit 1
cat "$FAKE/config.printed"
EOF
chmod +x "$FAKEBIN/ss" "$FAKEROOT/bin/telltale"

# A tool directory without `ss`, for the machine that doesn't have it.
NOSS="$T/noss"
mkdir -p "$NOSS"
for tool in awk tr sleep cat sed date; do ln -s "$(command -v "$tool")" "$NOSS/$tool"; done

DEFAULT_CONFIG='config_version = 1

[[listen]]
proto = "udp"
addr = "0.0.0.0:53"
proxy_protocol = false

[[listen]]
proto = "tcp"
addr = "0.0.0.0:53"
proxy_protocol = false

[[listen]]
proto = "udp"
addr = "[::]:53"
proxy_protocol = false

[[listen]]
proto = "tcp"
addr = "[::]:53"
proxy_protocol = false

[api]
listen = "0.0.0.0:8053"
'
STUB_UDP='UNCONN 0      0       127.0.0.53%lo:53 0.0.0.0:*'
STUB_TCP='LISTEN 0      4096    127.0.0.53%lo:53 0.0.0.0:*'
STUB_WHO='udp UNCONN 0      0       127.0.0.53%lo:53 0.0.0.0:* users:(("systemd-resolve",pid=612,fd=13))
tcp LISTEN 0      4096    127.0.0.53%lo:53 0.0.0.0:* users:(("systemd-resolve",pid=612,fd=14))'

# fixture BUSY [CONFIG [UDP_LINE TCP_LINE WHO]]: resets the fakes for one case.
fixture() {
  rm -f "$FAKE"/checks "$FAKE"/ss_fails "$FAKE"/config_fails
  printf '%s\n' "$1" > "$FAKE/busy_checks"
  printf '%s\n' "${2-$DEFAULT_CONFIG}" > "$FAKE/config.printed"
  printf '%s\n' "${3-$STUB_UDP}" > "$FAKE/udp_holder"
  printf '%s\n' "${4-$STUB_TCP}" > "$FAKE/tcp_holder"
  printf '%s\n' "${5-$STUB_WHO}" > "$FAKE/who"
  echo "# the installed config (the fake binary ignores it)" > "$T/telltale.toml"
}
checks() { cat "$FAKE/checks" 2>/dev/null || echo 0; }

# run_wait [WAIT_SECONDS] [PATH]: runs wait_for_port53 the way install.sh does (POSIX sh, set -eu);
# sets RC, OUT and MS (milliseconds it took).
run_wait() {
  local start end
  start=$(date +%s%N)
  set +e
  OUT=$(PATH="${2:-$FAKEBIN:$PATH}" FAKE="$FAKE" PREFIX="$FAKEROOT" TELLTALE_PORT_WAIT="${1:-}" \
    "$SH" -c 'set -eu; say() { printf "==> %s\n" "$*"; }; . "$1"; wait_for_port53 "$2"' sh "$T/lib.sh" "$T/telltale.toml" 2>&1)
  RC=$?
  set -e
  end=$(date +%s%N)
  MS=$(((end - start) / 1000000))
}
has() { printf '%s\n' "$OUT" | grep -qF -- "$1"; }

# ---- the wait functions come out of install.sh -------------------------------------------------
sed -n '/^# BEGIN port53-wait/,/^# END port53-wait/p' "$INSTALL" > "$T/lib.sh"
[ -s "$T/lib.sh" ] || fail "ops_004_port53_wait: $INSTALL has no port53-wait block (the wait for port 53)"
"$SH" -n "$T/lib.sh" || fail "ops_004_port53_wait: the block isn't valid sh"

# ---- 1. free at once: nothing to say, no waiting -----------------------------------------------
fixture 0
run_wait
[ "$RC" = 0 ] || fail "ops_004_port53_free_at_once: rc=$RC: $OUT"
[ -z "$OUT" ] || fail "ops_004_port53_free_at_once: said something on a free port: $OUT"
[ "$(checks)" = 1 ] || fail "ops_004_port53_free_at_once: $(checks) checks, not 1"
[ "$MS" -lt 2000 ] || fail "ops_004_port53_free_at_once: took $MS ms"
ok ops_004_port53_free_at_once
# The default config binds each address twice (UDP and TCP): it is named once.
got=$(FAKE="$FAKE" PREFIX="$FAKEROOT" "$SH" -c 'set -eu; . "$1"; port53_binds "$2"' sh "$T/lib.sh" "$T/telltale.toml" | tr '\n' ' ')
[ "$got" = "0.0.0.0 :: " ] || fail "ops_004_port53_binds_each_address_once: got '$got'"
ok ops_004_port53_binds_each_address_once

# ---- 2. held for the first 3 checks, then free: waits for it, no longer ------------------------
fixture 3
run_wait
[ "$RC" = 0 ] || fail "ops_004_port53_wait_ends_when_free: rc=$RC: $OUT"
[ "$(checks)" = 4 ] || fail "ops_004_port53_wait_ends_when_free: $(checks) checks, expected 4 (3 held + 1 free): $OUT"
[ "$MS" -ge 700 ] || fail "ops_004_port53_wait_ends_when_free: returned after $MS ms, before three polling intervals"
[ "$MS" -lt 8000 ] || fail "ops_004_port53_wait_ends_when_free: took $MS ms"
has "Waiting" || fail "ops_004_port53_wait_ends_when_free: no 'Waiting' line: $OUT"
has "Port 53 is free" || fail "ops_004_port53_wait_ends_when_free: no 'free' line: $OUT"
ok ops_004_port53_wait_ends_when_free

# ---- 3. never freed: bounded by the limit, names the holder and the next step, fails (rc 1) ----
fixture always
run_wait 2
[ "$RC" = 1 ] || fail "ops_004_port53_wait_is_bounded: rc=$RC, expected 1: $OUT"
[ "$MS" -ge 1900 ] || fail "ops_004_port53_wait_is_bounded: gave up after $MS ms, before the 2 s limit"
[ "$MS" -lt 12000 ] || fail "ops_004_port53_wait_is_bounded: took $MS ms for a 2 s limit"
has "still held after 2s" || fail "ops_004_port53_wait_is_bounded: no 'still held' line: $OUT"
has 'systemd-resolve' || fail "ops_004_port53_wait_is_bounded: the holder's process isn't named: $OUT"
has "sudo systemctl restart telltale" || fail "ops_004_port53_wait_is_bounded: no next step: $OUT"
ok ops_004_port53_wait_is_bounded

# ---- 4. a bad limit falls back to the default (15 s); the wait still ends when the port frees --
fixture 1
run_wait abc
[ "$RC" = 0 ] && has "up to 15s" || fail "ops_004_port53_wait_default_limit: rc=$RC: $OUT"
ok ops_004_port53_wait_default_limit

# ---- 5. no `ss` on the machine: one line, no waiting, no failure -------------------------------
fixture always
run_wait "" "$NOSS"
[ "$RC" = 0 ] || fail "ops_004_port53_wait_without_ss: rc=$RC: $OUT"
has "not checking that port 53 is free" || fail "ops_004_port53_wait_without_ss: no explanation: $OUT"
[ "$(checks)" = 0 ] || fail "ops_004_port53_wait_without_ss: ss was called"
[ "$(printf '%s\n' "$OUT" | wc -l)" = 1 ] || fail "ops_004_port53_wait_without_ss: more than one line: $OUT"
ok ops_004_port53_wait_without_ss

# ---- 6. an `ss` that can't list (too old for -H): the same --------------------------------------
fixture always
touch "$FAKE/ss_fails"
run_wait
[ "$RC" = 0 ] || fail "ops_004_port53_wait_ss_cannot_list: rc=$RC: $OUT"
has "not checking that port 53 is free" || fail "ops_004_port53_wait_ss_cannot_list: no explanation: $OUT"
[ "$MS" -lt 2000 ] || fail "ops_004_port53_wait_ss_cannot_list: waited ($MS ms)"
ok ops_004_port53_wait_ss_cannot_list

# ---- 7. which addresses count: only the ones TelltaleDNS binds ---------------------------------
LAN_CONFIG='config_version = 1

[[listen]]
proto = "udp"
addr = "192.0.2.10:53"

[[listen]]
proto = "tcp"
addr = "192.0.2.10:53"

[[listen]]
proto = "dot"
addr = "0.0.0.0:853"

[listen.tls]
cert = "/c.pem"
key = "/k.pem"
'
# (a) the stub on 127.0.0.53 doesn't conflict with a listener on the LAN address alone.
fixture always "$LAN_CONFIG"
run_wait
[ "$RC" = 0 ] && [ -z "$OUT" ] && [ "$(checks)" = 1 ] || fail "ops_004_port53_wait_other_address: rc=$RC checks=$(checks): $OUT"
ok ops_004_port53_wait_other_address
# (b) a wildcard holder (dual-stack `*:53`) does.
fixture 1 "$LAN_CONFIG" 'UNCONN 0      0              *:53       *:*' 'LISTEN 0      4096       *:53       *:*'
run_wait
[ "$RC" = 0 ] && has "Waiting" && [ "$(checks)" = 2 ] || fail "ops_004_port53_wait_wildcard_holder: rc=$RC checks=$(checks): $OUT"
ok ops_004_port53_wait_wildcard_holder
# (c) the same address does (IPv6, with brackets).
V6_CONFIG='config_version = 1

[[listen]]
proto = "udp"
addr = "[2001:db8::53]:53"
'
fixture 1 "$V6_CONFIG" 'UNCONN 0      0   [2001:db8::53]:53    [::]:*' 'LISTEN 0      4096   [2001:db8::53]:53    [::]:*'
run_wait
[ "$RC" = 0 ] && has "Waiting" && [ "$(checks)" = 2 ] || fail "ops_004_port53_wait_same_address: rc=$RC checks=$(checks): $OUT"
ok ops_004_port53_wait_same_address
# (d) a config with no port-53 listener at all: say so, don't wait, don't even look.
fixture always 'config_version = 1

[[listen]]
proto = "udp"
addr = "127.0.0.1:5353"
'
run_wait
[ "$RC" = 0 ] && has "no listener on port 53" && [ "$(checks)" = 0 ] || fail "ops_004_port53_wait_not_port_53: rc=$RC checks=$(checks): $OUT"
ok ops_004_port53_wait_not_port_53
# (e) a config the binary can't print: assume every address (the default), so wait.
fixture 1
touch "$FAKE/config_fails"
run_wait
[ "$RC" = 0 ] && has "Waiting" && [ "$(checks)" = 2 ] || fail "ops_004_port53_wait_unreadable_config: rc=$RC checks=$(checks): $OUT"
ok ops_004_port53_wait_unreadable_config

# ---- 8. the real binary prints the effective config the way install.sh reads it ----------------
REAL=${1:-}
if [ -n "$REAL" ] && [ -x "$REAL" ]; then
  mkdir -p "$T/real/bin"
  ln -s "$(cd "$(dirname "$REAL")" && pwd)/$(basename "$REAL")" "$T/real/bin/telltale"
  binds() { PREFIX="$T/real" "$SH" -c 'set -eu; . "$1"; port53_binds "$2"' sh "$T/lib.sh" "$1" | tr '\n' ' '; }
  echo 'config_version = 1' > "$T/c-default.toml"
  [ "$(binds "$T/c-default.toml")" = "0.0.0.0 :: " ] || fail "ops_004_port53_binds_real_default: got '$(binds "$T/c-default.toml")'"
  printf '%s\n' 'config_version = 1' '[[listen]]' 'proto = "udp"' 'addr = "192.0.2.10:53"' \
    '[[listen]]' 'proto = "tcp"' 'addr = "[2001:db8::53]:53"' \
    '[[listen]]' 'proto = "udp"' 'addr = "127.0.0.1:5353"' > "$T/c-lan.toml"
  [ "$(binds "$T/c-lan.toml")" = "192.0.2.10 2001:db8::53 " ] || fail "ops_004_port53_binds_real_lan: got '$(binds "$T/c-lan.toml")'"
  printf '%s\n' 'config_version = 1' '[[listen]]' 'proto = "udp"' 'addr = "127.0.0.1:5353"' > "$T/c-5353.toml"
  [ -z "$(binds "$T/c-5353.toml")" ] || fail "ops_004_port53_binds_real_no_53: got '$(binds "$T/c-5353.toml")'"
  ok ops_004_port53_binds_real_binary
else
  echo "skipped: no telltale binary given (the effective-config format is checked in CI)"
fi

# ---- 9. the service is started once -------------------------------------------------------------
# `enable --now` and then `restart` two milliseconds apart spent two of systemd's five starts per
# ten seconds and killed the first instance before it handled SIGTERM. One `restart` starts a
# stopped service and swaps in a new binary under a running one.
starts=$(grep -cE '^[[:space:]]*systemctl[[:space:]]+(enable[^#|]*--now|start|restart)[[:space:]]+telltale([[:space:]]|$)' "$INSTALL" || true)
[ "$starts" = 1 ] || fail "ops_004_install_starts_once: $INSTALL has $starts commands that start telltale, expected 1"
grep -qE '^[[:space:]]*systemctl[[:space:]]+restart[[:space:]]+telltale([[:space:]]|$)' "$INSTALL" \
  || fail "ops_004_install_starts_once: the one start isn't a restart"
grep -qE '^[[:space:]]*systemctl[[:space:]]+enable[[:space:]]+telltale([[:space:]>]|$)' "$INSTALL" \
  || fail "ops_004_install_starts_once: the service isn't enabled (without --now)"
ok ops_004_install_starts_once

echo "PASS ($n checks)"
