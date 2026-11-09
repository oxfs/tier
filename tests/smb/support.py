"""Connections and process configuration shared by real SMB integration tests."""

import json
import os
import re
import urllib.request
from pathlib import Path

import smbclient
from smbprotocol.exceptions import SMBResponseException


class Environment:
    def __init__(self):
        self.root = Path(os.environ["SMB_TEST_ROOT"])
        self.base = r"\\127.0.0.1\tier"
        self.config = dict(
            username="root", password="tier-integration-test", port=int(os.environ["SMB_TEST_PORT"])
        )
        smbclient.register_session("127.0.0.1", **self.config)

    def path(self, name):
        return self.base + "\\" + name.replace("/", "\\")

    def read(self, name, options=None):
        with smbclient.open_file(
            self.path(name), "rb", buffering=0, share_access="rwd", **(options or self.config)
        ) as stream:
            return stream.read()

    def write(self, name, data, options=None):
        with smbclient.open_file(
            self.path(name), "wb", buffering=0, **(options or self.config)
        ) as stream:
            offset = 0
            while offset < len(data):
                written = stream.write(data[offset:])
                if written <= 0:
                    raise OSError("SMB write made no progress")
                offset += written
            stream.flush()

    def action(self, name, values):
        match = re.search(r"address=(127\.0\.0\.1:\d+)", (self.root / "tier.log").read_text())
        if match is None:
            raise RuntimeError("Tier did not report its admin listen address")
        admin = "http://" + match.group(1)
        payload = json.dumps({"action": name, "input": values}).encode()
        request = urllib.request.Request(
            admin + "/api/action",
            data=payload,
            headers={"Content-Type": "application/json", "Origin": admin},
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)

    @staticmethod
    def close():
        try:
            smbclient.reset_connection_cache()
        except SMBResponseException:
            pass  # A backend-death test deliberately leaves disconnected trees.
