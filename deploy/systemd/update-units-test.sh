#!/usr/bin/env bash
# REQ: OPS-014 (ADR-182) — install.sh writes the node's data directory into the root update units:
# the path unit watches <data_dir>/update/request.json, and the service reads the request there and
# may write only that folder, the binary's directory and its own state. The data directory comes
# from the config's effective [node] data_dir (the default when it names none); one that isn't a
# plain absolute path gets no units. No root, nothing installed: the `update-units` block is cut
# out of install.sh and run under `sh` with the real binary (to print the effective config).
# Usage: deploy/systemd/update-units-test.sh path/to/telltale
set -euo pipefail
BIN="$(realpath "${1:?usage: update-units-test.sh path/to/telltale}")"
cd "$(dirname "$0")/../.."
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
fail() { echo "FAIL: $*"; exit 1; }
n=0
ok() { n=$((n + 1)); echo "ok $1"; }

mkdir -p "$T/prefix/bin" "$T/units"
cp "$BIN" "$T/prefix/bin/telltale"
sed -n '/^# BEGIN update-units/,/^# END update-units/p' deploy/systemd/install.sh > "$T/block.sh"
grep -q 'update_data_dir()' "$T/block.sh" || fail "the update-units block isn't in install.sh"
# Runs the block's functions in `sh` (as install.sh does): `data <config>` prints the data
# directory; `render <data_dir>` writes the units into $T/units.
run() {
  PREFIX="$T/prefix" sh -c '. "$1"; shift; case "$1" in
    data) update_data_dir "$2" ;;
    render) render_update_units deploy/systemd "$3" "$2" ;;
  esac' sh "$T/block.sh" "$@"
}
cfg() { # name [data_dir]
  { echo 'config_version = 1'
    if [ -n "${2:-}" ]; then printf '[node]\ndata_dir = "%s"\n' "$2"; fi
  } > "$T/$1.toml"
  echo "$T/$1.toml"
}

d=$(run data "$(cfg default)") || fail "the default config: no data directory"
[ "$d" = /var/lib/telltale ] || fail "the default data directory is $d"
ok "a config without [node] data_dir: /var/lib/telltale"

d=$(run data "$(cfg other /srv/telltale-data/)") || fail "a plain data directory refused"
[ "$d" = /srv/telltale-data ] || fail "the data directory is $d (the trailing / goes)"
ok "[node] data_dir = \"/srv/telltale-data/\": /srv/telltale-data"
run render "$d" "$T/units"
path="$T/units/telltale-update.path" svc="$T/units/telltale-update.service"
grep -qx 'PathExists=/srv/telltale-data/update/request.json' "$path" || fail "path unit: $(grep PathExists "$path")"
grep -qx "ExecStart=$T/prefix/bin/telltale self-update --request /srv/telltale-data/update/request.json --restart" "$svc" \
  || fail "service: $(grep ExecStart "$svc")"
grep -qx "ReadWritePaths=$T/prefix/bin -/srv/telltale-data/update" "$svc" || fail "service: $(grep ReadWritePaths "$svc")"
[ "$(grep -c '^ReadWritePaths=' "$svc")" = 1 ] || fail "more than one ReadWritePaths"
if cat "$path" "$svc" | grep -v '^#' | grep -q '/var/lib/telltale/'; then fail "the default data directory is left in a unit"; fi
ok "the units watch and write only /srv/telltale-data/update/, and run the binary from the prefix"

for bad in "/srv/my data" "relative/dir" "/srv/../etc" "/srv/./x" "/srv/tt;rm" '/srv/$HOME'; do
  if run data "$(cfg bad "$bad")" >/dev/null; then fail "data_dir \"$bad\" was taken"; fi
done
ok "a data directory that isn't a plain absolute path gets no units"

echo "PASS ($n checks)"
