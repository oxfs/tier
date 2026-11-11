"""Exercise the VFS IPC client and asynchronous timeout through a real smbd."""

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


def rpc_only_hello(message):
    # These single-connection proxies test RPC framing/failure handling. Disable
    # local access so reads cannot bypass the deliberately faulty transport.
    if message[:2] == b"\x08\x01":
        return message + b"\xb0\x01\x00"  # Request.local_access (22) = false.
    return message


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
            "version": lambda request_id: HEADER.pack(MAGIC, 2, 0, request_id, 0, 0),
            "flags": lambda request_id: HEADER.pack(MAGIC, 1, 1, request_id, 0, 0),
            "unexpected_data": lambda request_id: frame(request_id, data=b"x"),
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

    def test_fragmented_replies_preserve_file_content(self):
        # Force the header, protobuf and raw READ data across separate writes.
        # TCP/Unix streams can still coalesce writes; unit tests cover every
        # parser boundary, while this verifies the real tevent integration.
        def proxy(front, stop):
            with socket.socket(socket.AF_UNIX) as back:
                back.connect(str(self.root / "tier.sock"))
                back.settimeout(5)
                while not stop.is_set():
                    for source in select.select([front, back], [], [], 0.1)[0]:
                        request_id, message, data = receive_frame(source)
                        reply = frame(request_id, message, data)
                        if source is front:
                            back.sendall(frame(request_id, rpc_only_hello(message), data))
                        else:
                            cuts = (1, HEADER.size - 1, HEADER.size, HEADER.size + len(message))
                            offset = 0
                            for end in cuts:
                                if end > offset:
                                    front.sendall(reply[offset:end])
                                    offset = end
                            front.sendall(reply[offset:])

        output = self.root / "fragmented-output"
        with backend(self.root, proxy):
            result = self.client(f"get timeout-probe {output}")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(output.read_bytes(), (self.root / "input").read_bytes())

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
                            back.sendall(frame(request_id, rpc_only_hello(message), data))
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

    def test_metadata_close_does_not_wait_and_errors_fail_connection(self):
        for inject_error, single_slot in ((False, False), (True, False), (False, True)):
            with self.subTest(inject_error=inject_error, single_slot=single_slot):
                advanced = threading.Event()
                intercepted = threading.Event()

                def proxy(
                    front,
                    stop,
                    advanced=advanced,
                    intercepted=intercepted,
                    single_slot=single_slot,
                    inject_error=inject_error,
                ):
                    close_id = None
                    hello_id = None
                    held = None
                    with socket.socket(socket.AF_UNIX) as back:
                        back.connect(str(self.root / "tier.sock"))
                        back.settimeout(5)
                        while not stop.is_set():
                            for source in select.select([front, back], [], [], 0.1)[0]:
                                request_id, message, data = receive_frame(source)
                                if source is front:
                                    if message[:2] == b"\x08\x01":
                                        hello_id = request_id
                                    if close_id is not None:
                                        advanced.set()
                                        if held is not None:
                                            front.sendall(held)
                                            held = None
                                    elif message[:2] == b"\x08\x0e":
                                        # Listing opens only metadata pathrefs.
                                        close_id = request_id
                                        intercepted.set()
                                    back.sendall(frame(request_id, rpc_only_hello(message), data))
                                elif request_id == hello_id and single_slot:
                                    # A final scalar occurrence overrides HELLO's
                                    # max_inflight: the next call must drain CLOSE.
                                    front.sendall(frame(request_id, message + b"\x78\x01", data))
                                elif request_id == close_id:
                                    if inject_error:
                                        # The daemon released the handle; inject an
                                        # error in the asynchronous acknowledgement.
                                        message = b"\x08\x05"  # errno EIO
                                    reply = frame(request_id, message, data)
                                    if single_slot or advanced.is_set():
                                        front.sendall(reply)
                                    else:
                                        held = reply
                                else:
                                    front.sendall(frame(request_id, message, data))

                with backend(self.root, proxy):
                    result = self.client("ls; ls")
                self.assertTrue(intercepted.is_set(), result.stdout + result.stderr)
                self.assertTrue(advanced.is_set(), "CLOSE blocked the next IPC request")
                if inject_error:
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn(
                        "tier deferred CLOSE failed:", (self.root / "smb.log").read_text()
                    )
                else:
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
