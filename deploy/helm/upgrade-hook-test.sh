#!/usr/bin/env bash
# REQ: OPS-014 (ADR-182) — the chart's upgrade hook: off by default; on, a pre-upgrade Job that
# runs `ctl upgrade --to-self` from the same image as the controller, with the token from the
# named Secret; it refuses to render without one; and a NetworkPolicy that limits the API lets
# the hook in.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
fail=0
ok() { echo "ok:   $*"; }
bad() { echo "FAIL: $*"; fail=1; }
render() { helm template up "$here/telltale" "$@"; }
on=(--set upgradeHook.enabled=true --set upgradeHook.tokenSecret.name=tt-upgrade)

if render | grep -q 'helm.sh/hook'; then bad "a hook renders by default"; else ok "no hook by default"; fi

if out=$(render --set upgradeHook.enabled=true 2>&1); then
  bad "renders without upgradeHook.tokenSecret.name"
elif grep -q 'needs upgradeHook.tokenSecret.name' <<<"$out"; then
  ok "refuses to render without a token Secret"
else
  bad "unexpected error: $out"
fi

job=$(render "${on[@]}" --set image.tag=sha-abc1234 --show-only templates/upgrade-hook.yaml)
sts=$(render "${on[@]}" --set image.tag=sha-abc1234 --show-only templates/statefulset.yaml)
grep -q '"helm.sh/hook": pre-upgrade' <<<"$job" && ok "a pre-upgrade hook" || bad "not a pre-upgrade hook"
grep -q -- '- --to-self' <<<"$job" && grep -q -- '- --skip-if-unreachable' <<<"$job" \
  && ok "ctl upgrade --to-self --skip-if-unreachable" || bad "args: $job"
grep -q 'secretKeyRef: { name: tt-upgrade, key: token }' <<<"$job" && ok "the token from the Secret" || bad "token"
grep -q '"http://up-telltale-api.default.svc:8053"' <<<"$job" && ok "the release's API Service" || bad "url"
img_job=$(grep -m1 'image:' <<<"$job" | awk '{print $2}')
img_sts=$(grep -m1 'image:' <<<"$sts" | awk '{print $2}')
[ "$img_job" = "$img_sts" ] && ok "the controller's image ($img_job)" || bad "image $img_job vs $img_sts"
grep -q -- '--skip-down' <<<"$job" && bad "--skip-down by default" || ok "no --skip-down by default"
render "${on[@]}" --set upgradeHook.skipDown=true --show-only templates/upgrade-hook.yaml | grep -q -- '- --skip-down' \
  && ok "skipDown passes --skip-down" || bad "skipDown"

# REQ: OPS-014 (ADR-182) — whenUnavailable: `fail` (the default) keeps a sync blocked while a
# native node can't fetch the image's edge build; `skip` passes --skip-unavailable; nothing else.
grep -q -- '--skip-unavailable' <<<"$job" && bad "--skip-unavailable by default" || ok "whenUnavailable: fail by default"
render "${on[@]}" --set upgradeHook.whenUnavailable=skip --show-only templates/upgrade-hook.yaml | grep -q -- '- --skip-unavailable' \
  && ok "whenUnavailable: skip passes --skip-unavailable" || bad "whenUnavailable: skip"
if render "${on[@]}" --set upgradeHook.whenUnavailable=maybe >/dev/null 2>&1; then
  bad "whenUnavailable takes any value"
else
  ok "whenUnavailable is fail or skip (values.schema.json)"
fi

np=(--set networkPolicy.enabled=true)
render "${on[@]}" "${np[@]}" --set 'networkPolicy.apiFrom={192.168.1.0/24}' --show-only templates/networkpolicy.yaml \
  | grep -q 'app.kubernetes.io/component: upgrade-hook' && ok "a limited API lets the hook in" || bad "networkpolicy"
render "${on[@]}" "${np[@]}" --show-only templates/networkpolicy.yaml | grep -q 'upgrade-hook' \
  && bad "an extra rule for an open API" || ok "no extra rule when the API is open"
# Off (the default): no hook Job, and a limited API gets no rule for one.
render "${np[@]}" --set 'networkPolicy.apiFrom={192.168.1.0/24}' --show-only templates/networkpolicy.yaml | grep -q 'upgrade-hook' \
  && bad "a NetworkPolicy rule for a hook that is off" || ok "no NetworkPolicy rule while the hook is off"
render --set upgradeHook.tokenSecret.name=tt-upgrade | grep -q 'name: up-telltale-upgrade$' \
  && bad "a Job without upgradeHook.enabled" || ok "a token Secret alone renders no Job"

exit "$fail"
