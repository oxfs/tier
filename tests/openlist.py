#!/usr/bin/env python3
"""Real OpenList Local driver archive and SMB cold-read integration."""

import contextlib
import importlib.util
import json
import os
import socket
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "archive_smb", Path(__file__).with_name("archive-smb.py")
)
helpers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helpers)


def main():
    endpoint = os.environ["OPENLIST_URL"].rstrip("/")
    password = os.environ.get("OPENLIST_PASSWORD", "tier-test-password")

    def request(path, body, token=""):
        req = urllib.request.Request(
            endpoint + path,
            json.dumps(body).encode(),
            {"Content-Type": "application/json", "Authorization": token},
        )
        with urllib.request.urlopen(req, timeout=30) as response:
            result = json.load(response)
        assert result["code"] == 200, result
        return result.get("data")

    deadline = time.monotonic() + 60
    while True:
        try:
            token = request(
                "/api/auth/login", {"username": "admin", "password": password}
            )["token"]
            break
        except OSError:
            assert time.monotonic() < deadline, "OpenList startup timeout"
            time.sleep(0.5)
    storage = request(
        "/api/admin/storage/create",
        {
            "mount_path": "/archive",
            "driver": "Local",
            "addition": json.dumps({"root_folder_path": "/archive"}),
            "cache_expiration": 0,
            "webdav_policy": "proxy",
        },
        token,
    )
    os.environ["OPENLIST_TEST_USER"] = "admin"
    os.environ["OPENLIST_TEST_PASSWORD"] = password
    binary = os.environ.get("TIER_BIN", "/usr/local/bin/tier")
    with tempfile.TemporaryDirectory(prefix="tier-openlist-") as temporary:
        root = Path(temporary)
        for name in ("primary", "replica", "private", "lock", "state", "cache", "run"):
            (root / name).mkdir()
        config = root / "tier.toml"
        config.write_text(f'''primary = "{root}/primary"
[replicas]
backup = "{root}/replica"
[archives.remote]
url = "{endpoint}/dav/archive/"
username_env = "OPENLIST_TEST_USER"
password_env = "OPENLIST_TEST_PASSWORD"
[[backup]]
path = ""
count = 1
gc = true
[backup.archive]
count = 1
''')
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        smb = root / "smb.conf"
        smb.write_text(f"""[global]
server role = standalone server
interfaces = 127.0.0.1
bind interfaces only = yes
smb ports = {port}
private dir = {root}/private
lock directory = {root}/lock
state directory = {root}/state
cache directory = {root}/cache
pid directory = {root}/run
ncalrpc dir = {root}/run/ncalrpc
[tier]
path = {root}/primary
vfs objects = tier
tier:config = {config}
read only = no
force user = root
kernel share modes = no
posix locking = no
store dos attributes = no
ea support = no
map archive = no
""")
        helpers.run(
            "smbpasswd", "-s", "-c", str(smb), "-a", "root", input="test\ntest\n"
        )
        with helpers.management(binary, config) as api:
            process = subprocess.Popen(
                ["smbd", "-F", "--no-process-group", "-s", str(smb)]
            )
            try:
                deadline = time.monotonic() + 15
                while True:
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=1):
                            break
                    except OSError:
                        assert process.poll() is None and time.monotonic() < deadline
                        time.sleep(0.1)

                def client(command):
                    output = helpers.run(
                        "smbclient",
                        "//127.0.0.1/tier",
                        "-p",
                        str(port),
                        "-U",
                        "root%test",
                        "-c",
                        command,
                    )
                    assert "NT_STATUS_" not in output, output

                source = root / "input"
                source.write_bytes(b"OpenList archive roundtrip\n" * 8192)
                client(f"put {source} file.bin")
                api("scan")
                api("scan")
                assert not (root / "primary/file.bin").exists(), "GC did not complete"
                assert (root / "replica/.tier/meta/file.bin").is_file()
                destination = root / "output"
                client(f"get file.bin {destination}")
                assert source.read_bytes() == destination.read_bytes()
                storage_id = storage["id"]
                request(f"/api/admin/storage/disable?id={storage_id}", {}, token)
                failed = subprocess.run(
                    [
                        "smbclient",
                        "//127.0.0.1/tier",
                        "-p",
                        str(port),
                        "-U",
                        "root%test",
                        "-c",
                        f"get file.bin {destination}",
                    ],
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                assert (
                    failed.returncode or "NT_STATUS_" in failed.stdout + failed.stderr
                )
                request(f"/api/admin/storage/enable?id={storage_id}", {}, token)
                client(f"get file.bin {destination}")
                assert source.read_bytes() == destination.read_bytes()
                print("PASS: real OpenList PUT, verification, GC and SMB cold read")
            finally:
                with contextlib.suppress(ProcessLookupError):
                    process.terminate()
                process.wait(timeout=10)
            recovered = root / "recovered"
            recovered.mkdir()
            config.write_text(
                config.read_text().replace(
                    f'primary = "{root}/primary"', f'primary = "{recovered}"'
                )
            )
            api("recover-primary", {"confirmed": True})
            assert (recovered / "file.bin").read_bytes() == source.read_bytes()
            print(
                "PASS: OpenList unavailable/re-enabled and primary recovery from archive"
            )


if __name__ == "__main__":
    main()
