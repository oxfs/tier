#!/usr/bin/env python3
"""A missing replica must not interrupt healthy primary SMB access."""

import multiprocessing
import select
import socket
import socketserver
import sys
from pathlib import Path

from samba.credentials import Credentials
from samba.samba3 import libsmb_samba_internal as libsmb
from samba.samba3 import param


class Forward(socketserver.BaseRequestHandler):
    def handle(self):
        with socket.create_connection(("127.0.0.1", 14447)) as remote:
            while True:
                readable, _, _ = select.select([self.request, remote], [], [], 10)
                if not readable:
                    return
                for source in readable:
                    data = source.recv(65536)
                    if not data:
                        return
                    (remote if source is self.request else self.request).sendall(data)


class ForwardServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def main():
    replica = Path(sys.argv[1])
    disconnected = replica.with_name("detached-replica")
    # libsmb's Python binding uses port 445; isolate the forwarder on loopback .2.
    with ForwardServer(("127.0.0.2", 445), Forward) as server:
        process = multiprocessing.get_context("fork").Process(
            target=server.serve_forever
        )
        process.start()
        try:
            credentials = Credentials()
            settings = param.get_context()
            settings.load_default()
            credentials.guess(settings)
            credentials.set_username("root")
            credentials.set_password("test-password")
            credentials.set_domain("WORKGROUP")
            connection = libsmb.Conn("127.0.0.2", "tier", settings, credentials)
            handle = connection.create(
                "detach-test", DesiredAccess=0x0012019F, CreateDisposition=3
            )
            connection.write(handle, b"original", 0)
            assert connection.read(handle, 0, 8) == b"original"
            replica.rename(disconnected)
            try:
                assert connection.read(handle, 0, 8) == b"original"
                connection.write(handle, b"changed!", 0)
                fresh = connection.create(
                    "detach-new", DesiredAccess=0x0012019F, CreateDisposition=3
                )
                connection.write(fresh, b"new", 0)
                assert connection.read(fresh, 0, 3) == b"new"
                connection.close(fresh)
            finally:
                disconnected.rename(replica)
            assert connection.read(handle, 0, 8) == b"changed!"
            connection.close(handle)
            connection.unlink("detach-test")
            connection.unlink("detach-new")
        finally:
            process.terminate()
            process.join(timeout=5)


if __name__ == "__main__":
    main()
