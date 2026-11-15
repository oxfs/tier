#!/usr/bin/env bash
set -euo pipefail
umask 0007
[[ -f /etc/tier/config.toml ]] || {
    echo 'Missing /etc/tier/config.toml' >&2
    exit 1
}
case ${TIER_SMB_ENABLED:-1} in
    0 | 1) ;;
    *)
        echo 'TIER_SMB_ENABLED must be 0 or 1' >&2
        exit 1
        ;;
esac
if [[ ${TIER_SMB_ENABLED:-1} == 1 ]]; then
    # Bind the parent containing mounted disks, not an unverified empty directory.
    python3 - <<'PY'
import os, tomllib
with open('/etc/tier/config.toml', 'rb') as f:
    c = tomllib.load(f)
roots = [c['primary'], *c.get('replicas', {}).values()]
for root in roots:
    if not os.path.isdir(root) or not os.path.ismount(root):
        raise SystemExit(f'Storage must be a mounted disk: {root}')
PY
fi
if [[ -f /etc/tier/secrets ]]; then
    while IFS= read -r line || [[ -n $line ]]; do
        [[ -z $line || $line == \#* ]] && continue
        [[ $line =~ ^WEBDAV_[A-Z0-9_]+= ]] || {
            echo 'Secrets must use WEBDAV_ names' >&2
            exit 1
        }
        export "${line?}"
    done </etc/tier/secrets
fi
install -d -m 0700 /var/lib/samba/private
if [[ ${TIER_SMB_ENABLED:-1} == 1 ]] && ! pdbedit -L | cut -d: -f1 | grep -qx tier; then
    [[ -s /run/secrets/smb_password ]] || {
        echo 'Missing SMB password secret' >&2
        exit 1
    }
    password=$(cat /run/secrets/smb_password)
    printf '%s\n%s\n' "$password" "$password" | smbpasswd -s -a tier
    unset password
fi
export TIER_WEB_LISTEN=0.0.0.0:9999
export TIER_WEB_ASSETS=/usr/local/share/tier/web
export TMPDIR=/var/cache/tier
install -d -o tier -g tier -m 0770 "$TMPDIR"
children=()
# shellcheck disable=SC2317
stop() {
    trap - TERM INT
    kill -TERM "${children[@]}" 2>/dev/null || true
    wait "${children[@]}" 2>/dev/null || true
}
trap stop EXIT
trap 'exit 0' TERM INT
runuser -u tier -- tier /etc/tier/config.toml &
children+=("$!")
if [[ ${TIER_SMB_ENABLED:-1} == 1 ]]; then
    smbd --foreground --no-process-group --debug-stdout &
    children+=("$!")
fi
wait -n "${children[@]}"
exit 1
