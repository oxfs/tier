#!/usr/bin/env python3
"""Real SMB archive round trip; requires the installed tier module."""

import contextlib
import http.server
import json
import os
import signal
import socket
import subprocess
import tempfile
import threading
import time
from pathlib import Path


class Webdav(http.server.ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.objects = {}
        self.gets = 0
        self.fail = False
        self.guard = threading.Lock()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_port}/"


class Handler(http.server.BaseHTTPRequestHandler):
    def do_PUT(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        with self.server.guard:
            self.server.objects[self.path] = body
        self.send_response(201)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        with self.server.guard:
            self.server.gets += 1
            body = self.server.objects.get(self.path)
            fail = self.server.fail
        status = 503 if fail else 404 if body is None else 200
        body = body if status == 200 else b"unavailable"
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


@contextlib.contextmanager
def webdav():
    server = Webdav()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def run(*args, input=None):
    result = subprocess.run(
        args, input=input, text=True, capture_output=True, timeout=30, check=False
    )
    if result.returncode:
        raise AssertionError(f"{args}: {result.stdout}\n{result.stderr}")
    return result.stdout + result.stderr


def test_archive(directory, first, second):
    binary = os.environ.get(
        "TIER_BIN",
        str(Path(__file__).resolve().parents[1] / "target/release/tier"),
    )
    directory.chmod(0o755)
    for name in ("primary", "replica", "private", "lock", "state", "cache", "run"):
        (directory / name).mkdir()
    config = directory / "tier.toml"
    config.write_text(f"""primary = {json.dumps(str(directory / "primary"))}
[replicas]
backup = {json.dumps(str(directory / "replica"))}
[archives.first]
url = "{first.url}"
[archives.second]
url = "{second.url}"
[[backup]]
path = ""
count = 2
[[backup]]
path = "archive"
count = 2
gc = true
[backup.archive]
targets = ["first", "second"]
count = 2
""")
    run(binary, str(config), "init")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    smb = directory / "smb.conf"
    smb.write_text(f"""[global]
server role = standalone server
interfaces = 127.0.0.1
bind interfaces only = yes
smb ports = {port}
private dir = {directory}/private
lock directory = {directory}/lock
state directory = {directory}/state
cache directory = {directory}/cache
pid directory = {directory}/run
ncalrpc dir = {directory}/run/ncalrpc
log file = {directory}/smb.log
log level = 3
[tier]
path = {directory}/primary
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
    run(
        "smbpasswd",
        "-s",
        "-c",
        str(smb),
        "-a",
        "root",
        input="test-password\ntest-password\n",
    )
    client = [
        "smbclient",
        "//127.0.0.1/tier",
        "-p",
        str(port),
        "-U",
        "root%test-password",
    ]

    def smb_command(command, fail=False):
        result = subprocess.run(
            client + ["-c", command],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        output = result.stdout + result.stderr
        failed = result.returncode != 0 or "NT_STATUS_" in output
        assert failed == fail, f"{command}: {output}"
        return output

    with (directory / "process.log").open("w") as log:
        process = subprocess.Popen(
            ["smbd", "-F", "--no-process-group", "-s", str(smb)],
            stdout=log,
            stderr=log,
            start_new_session=True,
        )
        try:
            deadline = time.monotonic() + 10
            while True:
                assert process.poll() is None, (
                    "smbd exited before accepting connections"
                )
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                        break
                except OSError:
                    assert time.monotonic() < deadline, "smbd startup timeout"
                    time.sleep(0.05)
            source = directory / "input"
            source.write_bytes(b"archived through SMB\n" * 4096)
            smb_command(
                f"mkdir archive; put {source} archive/file.bin; put {source} local.bin"
            )
            run(binary, str(config), "scan")
            assert not (directory / "primary/archive/file.bin").exists()
            assert (directory / "replica/.tier/meta/archive/file.bin").is_file()
            assert len(first.objects) == len(second.objects) == 1
            before = first.gets + second.gets
            # allinfo also requests shadow-copy data using READ_DATA OPEN;
            # directory and path listings exercise metadata without that open.
            smb_command("ls archive/*; ls archive/file.bin")
            assert first.gets + second.gets == before, (
                "listing/stat downloaded an archive"
            )
            destination = directory / "download"
            smb_command(f"get archive/file.bin {destination}")
            assert destination.read_bytes() == source.read_bytes()
            source.write_bytes(b"must not overwrite archived data")
            smb_command(f"put {source} archive/file.bin", fail=True)
            smb_command(f"get archive/file.bin {destination}")
            assert destination.read_bytes() == b"archived through SMB\n" * 4096
            with first.guard:
                first.fail = True
            smb_command(f"get archive/file.bin {destination}")
            assert destination.read_bytes() == b"archived through SMB\n" * 4096
            with second.guard:
                second.fail = True
            before = first.gets + second.gets
            smb_command("ls archive/*; ls archive/file.bin")
            assert first.gets + second.gets == before, (
                "offline archive stat attempted a download"
            )
            smb_command(f"get archive/file.bin {destination}", fail=True)
            smb_command(f"get local.bin {destination}")
            assert destination.read_bytes() == b"archived through SMB\n" * 4096
            print(
                "PASS: SMB archive replication, stat without recall, read-only, failover and local isolation"
            )
        finally:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=10)


def main():
    with (
        tempfile.TemporaryDirectory(prefix="tier-archive-smb-") as temporary,
        webdav() as first,
        webdav() as second,
    ):
        directory = Path(temporary)
        try:
            test_archive(directory, first, second)
        except Exception:
            for name in ("smb.log", "process.log"):
                path = directory / name
                if path.exists():
                    print(path.read_text(errors="replace"))
            raise


if __name__ == "__main__":
    main()
