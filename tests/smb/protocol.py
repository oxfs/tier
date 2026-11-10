"""Exercise the C IPC decoder and asynchronous timeout through a real smbd."""

import os
import select
import socket
import struct
import subprocess
import threading
import time
import unittest
from contextlib import contextmanager
from pathlib import Path

HEADER = struct.Struct(">IHHQII")
MAGIC = 0x54494552
LIMIT = 1024 * 1024


def frame(request_id, message=b"", data=b"", magic=MAGIC):
    return HEADER.pack(magic, 1, 0, request_id, len(message), len(data)) + message + data


def receive_frame(stream):
    def read(length):
        result = bytearray()
        while len(result) < length:
            chunk = stream.recv(length - len(result))
            if not chunk:
                raise EOFError("IPC peer closed before a full frame")
            result.extend(chunk)
        return bytes(result)

    header = read(HEADER.size)
    _, _, _, request_id, message_len, data_len = HEADER.unpack(header)
    if message_len > LIMIT or data_len > LIMIT:
        raise ValueError("oversized test IPC frame")
    return request_id, read(message_len), read(data_len)


@contextmanager
def backend(root, responder):
    """One fake/proxy session, with bounded waits and exception propagation."""
    path = root / "fault.sock"
    path.unlink(missing_ok=True)
    errors = []
    stop = threading.Event()
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(str(path))
        listener.listen(1)
        listener.settimeout(0.1)

        def serve():
            try:
                while not stop.is_set():
                    try:
                        stream, _ = listener.accept()
                        break
                    except TimeoutError:
                        continue
                else:
                    return
                with stream:
                    stream.settimeout(75)
                    responder(stream, stop)
            except (BrokenPipeError, ConnectionResetError, EOFError):
                pass  # Rejected sessions are expected to close.
            except Exception as error:
                errors.append(error)

        worker = threading.Thread(target=serve)
        worker.start()
        try:
            yield
        finally:
            stop.set()
            worker.join(timeout=5)
            path.unlink(missing_ok=True)
            if worker.is_alive():
                raise RuntimeError("test IPC backend failed to stop")
            if errors:
                raise errors[0]


class ProtocolTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(os.environ["SMB_TEST_ROOT"])

    def client(self, command, timeout=15):
        return subprocess.run(
            [
                "smbclient",
                "//127.0.0.1/fault",
                # Allow Tier's 60-second deadline to fire before the client.
                "-t",
                "70",
                "-p",
                os.environ["SMB_TEST_PORT"],
                "-U",
                "root%tier-integration-test",
                "-s",
                str(self.root / "smb.conf"),
                "-c",
                command,
            ],
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )

    def test_malformed_responses_fail_and_are_logged(self):
        corruptions = {
            "magic": lambda request_id: frame(request_id, magic=0),
            "unknown_id": lambda request_id: frame(request_id + 1),
            "protobuf": lambda request_id: frame(request_id, b"\xff"),
            "length": lambda request_id: HEADER.pack(MAGIC, 1, 0, request_id, LIMIT + 1, 0),
            "truncated": lambda request_id: frame(request_id, b"\x60\x01")[:-1],
            # int32 errno = -1 encoded as a ten-byte varint.
            "negative_errno": lambda request_id: frame(request_id, b"\x08" + b"\xff" * 9 + b"\x01"),
        }
        for name, corrupt in corruptions.items():
            with self.subTest(name=name):

                def respond(stream, _stop, corrupt=corrupt):
                    request_id, _, _ = receive_frame(stream)
                    stream.sendall(corrupt(request_id))

                with backend(self.root, respond):
                    result = self.client("ls")
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("tier IPC failed:", (self.root / "smb.log").read_text())

    def test_async_read_timeout(self):
        # Relay all control operations to Tier, but withhold READ replies. The
        # connection stays open, so only the C async timer can complete the read.
        def proxy(front, stop):
            dropped = set()
            with socket.socket(socket.AF_UNIX) as back:
                back.connect(str(self.root / "tier.sock"))
                back.settimeout(5)
                while not stop.is_set():
                    for source in select.select([front, back], [], [], 0.1)[0]:
                        request_id, message, data = receive_frame(source)
                        if source is front:
                            if message[:2] == b"\x08\x0b":
                                dropped.add(request_id)
                            back.sendall(frame(request_id, message, data))
                        elif request_id not in dropped:
                            front.sendall(frame(request_id, message, data))

        started = time.monotonic()
        with backend(self.root, proxy):
            result = self.client(f"get timeout-probe {self.root / 'timeout-output'}", timeout=75)
        elapsed = time.monotonic() - started
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertGreaterEqual(elapsed, 55)
        self.assertLess(elapsed, 75)
        self.assertIn("tier IPC timeout:", (self.root / "smb.log").read_text())


if __name__ == "__main__":
    unittest.main(verbosity=2)
