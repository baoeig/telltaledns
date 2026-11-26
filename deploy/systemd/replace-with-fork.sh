#!/bin/sh
# REQ: OPS-004, OPS-009 — replace the binary of an existing native TelltaleDNS install.
set -eu
umask 077

binary=/usr/local/bin/telltale
config=/etc/telltale/telltale.toml
service=telltale.service
service_user=telltale
backup_root=/var/backups/telltale
build_commit=24f2f646
asset_ref=e1b151c152daf299fe940656a18f790aa8a176fb
assets="https://raw.githubusercontent.com/baoeig/telltaledns/$asset_ref"

work=
staged=
backup_dir=
rollback=no

die() { printf 'telltale replacement: %s\n' "$*" >&2; exit 1; }

fetch() {
    curl -fsSL --proto '=https' --proto-redir '=https' --retry 2 --connect-timeout 10 --max-time 180 -o "$2" "$1"
}

# The stock unit uses one TOML file. The API accepts HTTP health probes even with TLS on.
readiness_url() {
    awk '
        /^\[api\]$/ { section = "api"; next }
        /^\[telemetry\.metrics\]$/ { section = "metrics"; next }
        /^\[/ { section = ""; next }
        section != "" && $1 == "enabled" { enabled[section] = $3 }
        section != "" && $1 == "listen" { gsub(/"/, "", $3); listen[section] = $3 }
        END {
            if (enabled["api"] != "false") {
                address = listen["api"] != "" ? listen["api"] : "0.0.0.0:8053"
            } else if (enabled["metrics"] != "false") {
                address = listen["metrics"] != "" ? listen["metrics"] : "0.0.0.0:9153"
            } else {
                exit
            }
            sub(/^0\.0\.0\.0:/, "127.0.0.1:", address)
            sub(/^\[::\]:/, "[::1]:", address)
            print "http://" address "/readyz?startup=1"
        }'
}

ready() {
    systemctl is-active --quiet "$service" || return 1
    [ -z "$health_url" ] || [ "$(curl --noproxy '*' -s --max-time 2 -o /dev/null -w '%{http_code}' "$health_url")" = 200 ]
}

wait_ready() {
    left=20
    while [ "$left" -gt 0 ]; do
        if ready; then
            sleep 1
            ready && return 0
        fi
        left=$((left - 1))
        sleep 1
    done
    return 1
}

cleanup() {
    status=$?
    set +e
    trap - EXIT
    trap '' HUP INT TERM
    if [ "$rollback" = yes ]; then
        printf 'The new build did not complete startup. Restoring the previous binary.\n' >&2
        if cp -p "$backup_dir/telltale" "$staged" && mv -f "$staged" "$binary"; then
            systemctl reset-failed "$service" >/dev/null 2>&1 || :
            if systemctl restart "$service" && wait_ready; then
                printf 'Previous version restored and running.\n' >&2
            else
                printf 'Previous binary restored. Check journalctl -u %s.\n' "$service" >&2
            fi
        else
            printf 'Could not restore the binary. The backup is at %s/telltale.\n' "$backup_dir" >&2
        fi
        status=1
    fi
    if [ -n "$staged" ]; then rm -f "$staged"; fi
    if [ -n "$work" ]; then rm -rf "$work"; fi
    exit "$status"
}

main() {
    [ "$(id -u)" -eq 0 ] || die "run this script with sudo"
    [ "$(uname -s)" = Linux ] || die "the existing service must run on Linux"
    for tool in curl systemctl runuser flock sha256sum gzip; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
    done
    case "$(uname -m)" in
        x86_64|amd64) asset=telltale-x86_64-linux.gz ;;
        aarch64|arm64) asset=telltale-aarch64-linux.gz ;;
        armv7l|armv8l) asset=telltale-armv7-linux.gz ;;
        *) die "no binary for $(uname -m)" ;;
    esac
    case "$#" in
        0) health_url= ;;
        2) [ "$1" = --health-url ] || die "usage: $0 [--health-url URL]"; health_url=$2 ;;
        *) die "usage: $0 [--health-url URL]" ;;
    esac
    [ -x "$binary" ] && [ -r "$config" ] || die "the default binary or configuration was not found"
    [ "$(systemctl show --property=User --value "$service")" = "$service_user" ] ||
        die "expected a systemd service running as $service_user"
    case "$(systemctl show --property=ExecStart --value "$service")" in
        *"argv[]=$binary run -c $config ;"*) ;;
        *) die "expected the standard install.sh ExecStart command in $service" ;;
    esac

    exec 9>"$binary.replace.lock"
    flock -n 9 || die "another replacement is running"
    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    if [ -z "$health_url" ]; then
        effective=$(runuser -u "$service_user" -- "$binary" config check "$config" --no-env --print) ||
            die "the current configuration could not be read"
        health_url=$(printf '%s\n' "$effective" | readiness_url)
        unset effective
    fi
    ready || die "the current service is not ready at ${health_url:-its systemd unit}"
    old_version=$(runuser -u "$service_user" -- "$binary" --version)
    case "$old_version" in
        *"commit $build_commit,"*) printf 'This build is already installed: %s\n' "$old_version"; return ;;
    esac

    work=$(mktemp -d)
    # Use a separate cache key for the manifest when a new build is published.
    fetch "$assets/SHA256SUMS?commit=$asset_ref" "$work/SHA256SUMS"
    fetch "$assets/PARTS?commit=$asset_ref" "$work/PARTS"
    part_count=$(awk -v name="$asset" '$2 == name { print $1 }' "$work/PARTS")
    case "$part_count" in ''|*[!0-9]*) die "invalid part count for $asset" ;; esac
    [ "$part_count" -ge 1 ] && [ "$part_count" -le 99 ] || die "invalid part count for $asset"
    : > "$work/$asset"
    part=1
    while [ "$part" -le "$part_count" ]; do
        suffix=$(printf '%02d' "$part")
        fetch "$assets/$asset.part-$suffix" "$work/part"
        cat "$work/part" >> "$work/$asset"
        part=$((part + 1))
    done
    expected=$(awk -v name="$asset" '$2 == name { print $1 }' "$work/SHA256SUMS")
    actual=$(sha256sum "$work/$asset")
    [ "${actual%% *}" = "$expected" ] || die "checksum verification failed for $asset"

    staged=$(mktemp "$binary.new.XXXXXX")
    gzip -dc "$work/$asset" > "$staged"
    chmod 0755 "$staged"
    new_version=$(runuser -u "$service_user" -- "$staged" --version) || die "the candidate cannot run"
    case "$new_version" in *"commit $build_commit,"*) ;; *) die "unexpected build: $new_version" ;; esac
    runuser -u "$service_user" -- "$staged" config check "$config" ||
        die "the new binary rejected the existing configuration"

    mkdir -p "$backup_root"
    backup_dir=$(mktemp -d "$backup_root/pre-gfwlist.XXXXXX")
    cp -p "$binary" "$backup_dir/telltale"
    "$binary" backup create -c "$config" -o "$backup_dir/telltale.ttbk" ||
        die "the existing state could not be backed up"
    cmp -s "$binary" "$backup_dir/telltale" || die "the installed binary changed during the backup"

    rollback=yes
    mv -f "$staged" "$binary"
    if ! systemctl restart "$service" || ! wait_ready; then
        die "the new service did not become ready"
    fi
    rollback=no
    printf 'Installed %s\nBackup: %s\nOpen Filtering & DNS → DNS Routes in the web UI.\n' "$new_version" "$backup_dir"
}

main "$@"
