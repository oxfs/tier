#!/usr/bin/env bash
set -euo pipefail
root=$(mktemp -d)
installer=${1:-target/docker-package/tier/deploy/setup.sh}
cleanup() {
    if [[ -f "$root/install/compose.yml" ]]; then
        sudo docker compose -f "$root/install/compose.yml" logs || true
        sudo docker compose -f "$root/install/compose.yml" down -v || true
    fi
    sudo umount -R "$root/disks" || return
    sudo rm -rf "$root"
}
trap cleanup EXIT
mkdir -p "$root/disks/primary" "$root/disks/replica"
sudo mount --bind "$root/disks" "$root/disks"
sudo mount --make-rshared "$root/disks"
printf 'installer-test-password\n' >"$root/password"
# CI has no spare physical disks. Only this fixture maps its real loop devices
# to disks in lsblk's TYPE output; production must continue rejecting loops.
mkdir "$root/bin"
cat >"$root/bin/lsblk" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
/usr/bin/lsblk "$@" | sed 's/ loop$/ disk/'
SH
chmod 755 "$root/bin/lsblk"
setup() {
    sudo env PATH="$root/bin:$PATH" bash "$installer" --directory "$root/install" --primary "$root/disks/primary" \
        --replica "$root/disks/replica" --password-file "$root/password" --image tier:test --skip-pull
}
if setup >"$root/rejected" 2>&1; then
    echo 'Installer accepted unmounted directories' >&2
    exit 1
fi
grep -q 'Not a mounted data disk' "$root/rejected"
test ! -e "$root/install"
for disk in primary replica; do
    truncate -s 128M "$root/$disk.img"
    mkfs.ext4 -q "$root/$disk.img"
    sudo mount -o loop "$root/$disk.img" "$root/disks/$disk"
    sudo rmdir "$root/disks/$disk/lost+found"
done
sudo touch "$root/disks/primary/keep"
if setup >"$root/rejected" 2>&1; then
    echo 'Installer accepted existing data' >&2
    exit 1
fi
grep -q 'Disk is not empty' "$root/rejected"
test -f "$root/disks/primary/keep"
sudo rm "$root/disks/primary/keep"
sudo mount --bind "$root/disks/primary" "$root/disks/replica"
if setup >"$root/rejected" 2>&1; then
    echo 'Installer accepted two paths on one filesystem' >&2
    exit 1
fi
grep -q 'Primary and replica must be separate filesystems' "$root/rejected"
sudo umount "$root/disks/replica"
setup
test "$(sudo stat -c %a "$root/install/smb_password")" = 600
printf 'installer SMB roundtrip\n' >"$root/input"
smbclient //127.0.0.1/tier -U tier%installer-test-password -c "put $root/input file.bin; get file.bin $root/output"
cmp "$root/input" "$root/output"
curl --fail-with-body -sS -X POST -H 'X-Tier-Request: 1' -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:9999/api/scan
sudo cmp "$root/input" "$root/disks/replica/file.bin"
before=$(sudo sha256sum "$root/install/config/config.toml")
if setup >"$root/rejected" 2>&1; then
    echo 'Installer overwrote existing installation' >&2
    exit 1
fi
grep -q 'Installation directory already exists' "$root/rejected"
test "$before" = "$(sudo sha256sum "$root/install/config/config.toml")"
