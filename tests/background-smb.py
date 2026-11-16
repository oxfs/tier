"""A busy SMB rename cancels an audit, then succeeds after its current file."""

import concurrent.futures
import fcntl
import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

root = Path(sys.argv[1])
config = root / "config.toml"
original = config.read_text()


def scan():
    request = urllib.request.Request(
        "http://127.0.0.1:14448/api/scan",
        data=b"{}",
        headers={"X-Tier-Request": "1", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def smb(command, busy=False):
    result = subprocess.run(
        [
            "smbclient",
            "//127.0.0.1/tier",
            "-p",
            "14447",
            "-U",
            "root%test-password",
            "-c",
            command,
        ],
        capture_output=True,
        check=False,
        text=True,
        timeout=10,
    )
    output = result.stdout + result.stderr
    if busy:
        assert "NT_STATUS_" in output, output
    else:
        assert result.returncode == 0 and "NT_STATUS_" not in output, output


try:
    payload = root / "audit-input"
    payload.write_bytes(b"x" * (512 * 1024))
    smb("mkdir audit; " + "; ".join(f"put {payload} audit/{i}" for i in range(3)))
    report = scan()
    assert not report["errors"] and not report["interrupted"], report
    assert not (root / "primary/audit/0").exists()
    config.write_text(
        original + "\n[background]\nworkers=1\naudit_bytes_per_sec=262144\n"
    )
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        pending = pool.submit(scan)
        deadline = time.monotonic() + 10
        while True:
            with (root / "primary/.tier/meta/audit/0").open("rb") as descriptor:
                try:
                    fcntl.flock(descriptor, fcntl.LOCK_SH | fcntl.LOCK_NB)
                except BlockingIOError:
                    break
            assert not pending.done(), pending.result() if pending.done() else None
            assert time.monotonic() < deadline, "audit never locked the file"
            time.sleep(0.01)
        started = time.monotonic()
        smb("rename audit audit-moved", busy=True)
        assert time.monotonic() - started < 1, "SMB waited for the background file"
        report = pending.result(timeout=15)
    assert report["interrupted"] and report["completed"] == 1, report
    assert not report["errors"], report
    smb(f"rename audit audit-moved; get audit-moved/0 {root / 'audit-readback'}")
    assert (root / "audit-readback").read_bytes() == payload.read_bytes()
    smb("del audit-moved/*; rmdir audit-moved")
    for disk in ["primary", "replica"]:
        for directory in [
            "audit",
            "audit-moved",
            ".tier/meta/audit",
            ".tier/meta/audit-moved",
        ]:
            assert not (root / disk / directory).exists(), (disk, directory)
finally:
    config.write_text(original)

print("PASS: SMB busy rename interrupts audit; retry, read and delete after GC")
