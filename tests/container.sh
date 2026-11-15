#!/usr/bin/env bash
set -euo pipefail
root=$(mktemp -d)
name=tier-container-test
cleanup() {
    docker logs "$name" || true
    docker rm -f "$name" || true
    sudo umount "$root/disks/primary" || true
    sudo umount "$root/disks/replica" || true
    sudo umount "$root/disks" || true
    sudo rm -rf "$root"
}
trap cleanup EXIT
mkdir "$root/config" "$root/primary" "$root/replica" "$root/samba"
mkdir -p "$root/disks/primary" "$root/disks/replica"
sudo mount --bind "$root/disks" "$root/disks"
sudo mount --make-rshared "$root/disks"
sudo mount -t tmpfs -o mode=0777 tmpfs "$root/disks/primary"
sudo mount -t tmpfs -o mode=0777 tmpfs "$root/disks/replica"
chmod 777 "$root" "$root/config" "$root/primary" "$root/replica"
printf 'primary = "/srv/tier/primary"\n[replicas]\nbackup = "/srv/tier/replica"\n[[backup]]\npath = ""\ncount = 2\n' >"$root/config/config.toml"
printf 'test-password\n' >"$root/password"
sed 's/USERNAME/tier/' deploy/smb.conf.example >"$root/smb.conf"
start() {
    docker run -d --name "$name" -p 127.0.0.1:19999:9999 -p 127.0.0.1:1445:445 \
        -v "$root/config:/etc/tier" -v "$root/disks:/srv/tier:rslave" \
        -v "$root/password:/run/secrets/smb_password:ro" \
        -v "$root/samba:/var/lib/samba" -v "$root/smb.conf:/etc/samba/smb.conf:ro" tier:test
}
start
for _ in $(seq 1 30); do
    if curl -fsS -X POST -H 'X-Tier-Request: 1' -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:19999/api/init; then
        break
    fi
    sleep 1
done
printf 'container persistence\n' >"$root/input"
smbclient //127.0.0.1/tier -p 1445 -U tier%test-password -c "put $root/input file.bin"
curl -fsS -X POST -H 'X-Tier-Request: 1' -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:19999/api/scan
sudo cmp "$root/input" "$root/disks/replica/file.bin"
docker stop -t 10 "$name"
test "$(docker inspect -f '{{.State.ExitCode}}' "$name")" != 137
docker rm "$name"
# The existing account must survive recreation, without reading a new secret.
printf 'changed-password\n' >"$root/password"
start
sleep 3
smbclient //127.0.0.1/tier -p 1445 -U tier%test-password -c "get file.bin $root/output"
cmp "$root/input" "$root/output"
docker stop -t 10 "$name"
test "$(docker inspect -f '{{.State.ExitCode}}' "$name")" != 137
sudo umount "$root/disks/replica"
if docker run --rm -v "$root/config:/etc/tier" -v "$root/disks:/srv/tier:rslave" tier:test >"$root/rejection" 2>&1; then
    echo 'Missing mounts were accepted' >&2
    exit 1
fi
grep -q 'Storage must be a mounted disk' "$root/rejection"
docker rm "$name"
docker run -d --name "$name" -e TIER_SMB_ENABLED=0 -p 127.0.0.1:19999:9999 \
    -v "$root/config:/etc/tier" -v "$root/disks:/srv/tier:rslave" tier:test
for _ in $(seq 1 30); do
    if curl -fsS http://127.0.0.1:19999/api/health >"$root/health"; then
        break
    fi
    sleep 1
done
test -s "$root/health"
# A repair session exposes management without starting Samba on incomplete disks.
docker exec "$name" python3 -c 'import socket; s=socket.socket(); assert s.connect_ex(("127.0.0.1",445)) != 0'
