"""Real SMB2/3 behavior and failure tests against an installed Tier VFS module."""

import concurrent.futures
import csv
import errno
import os
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path

import smbclient
from smbclient._os import _set_basic_information
from smbprotocol.exceptions import SMBResponseException
from support import Environment


class ConnectedTest(unittest.TestCase):
    def setUp(self):
        self.env = Environment()
        self.addCleanup(self.env.close)


class SMBTests(ConnectedTest):
    def test_physical_names_and_mapping_export(self):
        self.env.write("physical-before.txt", b"portable contents")
        initial = list((self.env.root / "objects").rglob("*--physical-before.txt"))
        self.assertEqual(len(initial), 1)
        latest_name = "离线 名称 #%.txt"
        smbclient.rename(
            self.env.path("physical-before.txt"), self.env.path(latest_name), **self.env.config
        )
        self.env.action("repair", {})
        mapping = self.env.root / "files.csv"
        self.env.action("export", {"path": str(mapping)})
        with mapping.open(newline="") as stream:
            rows = csv.DictReader(stream)
            self.assertEqual(rows.fieldnames, ["logical_path", "physical_path"])
            current = [row for row in rows if row["logical_path"] == "/" + latest_name]
        self.assertEqual(len(current), 1)
        physical = Path(current[0]["physical_path"])
        self.assertTrue(physical.name.endswith("--" + latest_name))
        self.assertEqual(physical.name[:36], initial[0].name[:36])
        self.assertEqual(physical.read_bytes(), b"portable contents")
        self.assertFalse(initial[0].exists())

    def test_ordinary_user(self):
        # Exercise the production shared-group identity, without forcing an SMB user to root.
        user_options = dict(self.env.config, username="tier-test", connection_cache={})
        try:
            self.env.write("ordinary-user", b"group-authorized", user_options)
            assert self.env.read("ordinary-user", user_options) == b"group-authorized"
            smbclient.mkdir(self.env.path("ordinary-user-directory"), **user_options)
            smbclient.remove(self.env.path("ordinary-user"), **user_options)
            smbclient.rmdir(self.env.path("ordinary-user-directory"), **user_options)
        finally:
            smbclient.reset_connection_cache(connection_cache=user_options["connection_cache"])

    def test_io_and_attributes(self):
        smbclient.mkdir(self.env.path("attributes-directory"), **self.env.config)
        # An explicit flush followed by more writes must use the same valid handle.
        with smbclient.open_file(
            self.env.path("partial"), "w+b", buffering=0, **self.env.config
        ) as f:
            f.write(b"abcdefgh")
            f.flush()
            f.seek(2)
            f.write(b"XY")
            f.flush()
            f.seek(0)
            assert f.read() == b"abXYefgh"
            f.truncate(5)
            f.flush()
        assert self.env.read("partial") == b"abXYe"

        # Cross-session stat and read observe writes; no mutable attribute TTL cache.
        with smbclient.open_file(
            self.env.path("visible"), "wb", buffering=0, share_access="rwd", **self.env.config
        ) as f:
            f.write(b"visible-before-close")
            assert smbclient.stat(self.env.path("visible"), **self.env.config).st_size == 20
            assert self.env.read("visible") == b"visible-before-close"

        # Attributes preserve SMB's 100 ns precision; streams are explicitly unsupported.
        stamp = 1_704_164_645_123_456_700
        smbclient.utime(self.env.path("partial"), ns=(stamp, stamp), **self.env.config)
        assert smbclient.stat(self.env.path("partial"), **self.env.config).st_mtime_ns == stamp
        try:
            self.env.write("partial:private", b"unsupported stream")
        except OSError:
            pass
        else:
            raise AssertionError("named stream unexpectedly accepted")
        assert self.env.read("partial") == b"abXYe"
        # Ordinary Windows copy tools set the archive bit even when DOS storage is disabled.
        _set_basic_information(self.env.path("partial"), file_attributes=0x20, **self.env.config)
        _set_basic_information(
            self.env.path("attributes-directory"), file_attributes=0x10, **self.env.config
        )
        assert self.env.read("partial") == b"abXYe"

    def test_namespace(self):
        self.env.write("namespace-file", b"abXYe")
        # Unicode names, directory paging and case-insensitive name lookup.
        smbclient.mkdir(self.env.path("目录"), **self.env.config)
        root_entries = {
            entry.name: entry for entry in smbclient.scandir(self.env.base, **self.env.config)
        }
        assert root_entries["目录"].is_dir()
        assert root_entries["namespace-file"].is_file()
        for i in range(300):
            self.env.write(f"目录/entry-{i:04}", str(i).encode())
        assert set(smbclient.listdir(self.env.path("目录"), **self.env.config)) == {
            f"entry-{i:04}" for i in range(300)
        }
        self.env.write("MixedCase", b"case")
        assert self.env.read("mixedcase") == b"case"

        # Namespace errors propagate; a failed mutation cannot silently succeed.
        try:
            smbclient.rmdir(self.env.path("目录"), **self.env.config)
        except OSError as error:
            assert error.errno == errno.ENOTEMPTY, error
        else:
            raise AssertionError("nonempty directory removal succeeded")
        smbclient.rename(
            self.env.path("namespace-file"), self.env.path("renamed"), **self.env.config
        )
        assert self.env.read("renamed") == b"abXYe"
        try:
            smbclient.stat(self.env.path("namespace-file"), **self.env.config)
        except OSError as error:
            assert error.errno == errno.ENOENT, error
        else:
            raise AssertionError("old rename path remains visible")

    def test_concurrent_io(self):
        # Distinct-file I/O can proceed concurrently across SMB sessions/processes.
        def worker(index):
            data = bytes([index]) * (1024 * 1024 + 123)
            name = f"parallel-{index}"
            options = dict(self.env.config, connection_cache={})
            try:
                self.env.write(name, data, options)
                assert self.env.read(name, options) == data
            finally:
                smbclient.reset_connection_cache(connection_cache=options["connection_cache"])

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(worker, range(8)))

        # Several in-flight requests in one smbd connection exercise the tevent dispatcher.
        def shared_worker(index):
            data = bytes([index]) * 32768
            name = f"shared-{index}"
            self.env.write(name, data)
            assert self.env.read(name) == data

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(shared_worker, range(8)))

    def test_share_modes(self):
        # Samba retains share-mode enforcement even with kernel locking disabled.
        with smbclient.open_file(self.env.path("exclusive"), "wb", buffering=0, **self.env.config):
            try:
                self.env.read("exclusive", dict(self.env.config, connection_cache={}))
            except OSError as error:
                assert error.ntstatus == 0xC0000043, error  # STATUS_SHARING_VIOLATION
            else:
                raise AssertionError("exclusive SMB open was bypassed")

    def test_client_disconnect(self):
        # Abrupt client exit releases its writer in Tier, without restarting the daemon.
        child = subprocess.Popen(
            [sys.executable, __file__, "hold"], stdout=subprocess.PIPE, text=True
        )
        try:
            assert child.stdout.readline().strip() == "READY"
        finally:
            child.kill()
            child.wait(timeout=10)
        for attempt in range(100):
            try:
                self.env.write("abandoned", b"reopened")
                break
            except OSError:
                if attempt == 99:
                    raise
                time.sleep(0.1)
        assert self.env.read("abandoned") == b"reopened"

    def test_identity_and_repair(self):
        smbclient.mkdir(self.env.path("protected"), **self.env.config)
        replica = self.env.root / "replica"
        replica.mkdir()
        self.env.action(
            "target",
            {
                "name": "replica",
                "path": str(replica),
                "allow_same_domain": True,
                "domain": "test-replica",
            },
        )
        policy = {
            "path": "/protected",
            "write_cache": ["main"],
            "persistent_storage": ["replica"],
            "persistent_copies": 1,
            "persist_delay_secs": 0,
            "direct_fallback": False,
            "low_water": 99,
            "high_water": 100,
        }
        self.env.action("policy", policy)
        self.env.write("protected/saved", b"verified-long-term-data")
        self.env.write("protected/stale", b"old-replica")
        self.env.action("repair", {})
        policy["persist_delay_secs"] = 3600
        self.env.action("policy", policy)
        self.env.write("protected/stale", b"latest-unreplicated-content")
        primary = self.env.root / "objects"
        offline = self.env.root / "objects-offline"
        primary.rename(offline)
        primary.mkdir()
        try:
            assert self.env.read("protected/saved") == b"verified-long-term-data"
            try:
                self.env.read("protected/stale")
            except OSError:
                pass
            else:
                raise AssertionError("an obsolete replica was returned as current data")
            try:
                self.env.write("protected/no-target", b"must-fail")
            except OSError:
                pass
            else:
                raise AssertionError("write accepted without an eligible target")
            assert list(primary.iterdir()) == [], "replacement target was modified"
        finally:
            primary.rmdir()
            offline.rename(primary)
        assert self.env.read("protected/stale") == b"latest-unreplicated-content"
        # Failed creation may leave an empty namespace entry, like a failed ordinary write.
        try:
            smbclient.remove(self.env.path("protected/no-target"), **self.env.config)
        except OSError as error:
            assert error.errno == errno.ENOENT
        policy["persist_delay_secs"] = 0
        self.env.action("policy", policy)
        self.env.action("repair", {})
        corrupt = [
            p
            for p in replica.rglob("*")
            if p.is_file() and p.read_bytes() == b"verified-long-term-data"
        ]
        assert len(corrupt) == 1
        corrupt[0].write_bytes(b"corrupted-long-term-data")
        self.env.action("repair", {})
        assert any(
            p.is_file() and p.read_bytes() == b"verified-long-term-data" for p in replica.rglob("*")
        )
        assert self.env.read("protected/saved") == b"verified-long-term-data"


class ShutdownTests(ConnectedTest):
    def test_backend_death_and_graceful_shutdown(self):
        # A killed backend invalidates old handles; a fresh tree connection recovers.
        self.env.write("durable", b"survives daemon restart")
        old = smbclient.open_file(
            self.env.path("durable"), "rb", buffering=0, share_access="rwd", **self.env.config
        )
        os.kill(int(os.environ["TIER_TEST_PID"]), signal.SIGKILL)
        time.sleep(0.2)
        try:
            old.read(1)
        except (OSError, SMBResponseException):
            pass
        else:
            raise AssertionError("old handle survived backend death")
        try:
            old.close()
        except (OSError, SMBResponseException):
            pass
        try:
            smbclient.reset_connection_cache()
        except SMBResponseException:
            pass
        log = (self.env.root / "tier-restarted.log").open("wb")
        server = subprocess.Popen(
            [
                "setpriv",
                "--regid",
                os.environ["TIER_TEST_GID"],
                "--clear-groups",
                os.environ["TIER_BIN"],
                "--db",
                str(self.env.root / "metadata"),
                "serve",
                "--socket",
                str(self.env.root / "tier.sock"),
                "--admin",
                "127.0.0.1:0",
            ],
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        try:
            for _ in range(100):
                try:
                    smbclient.register_session("127.0.0.1", **self.env.config)
                    assert self.env.read("durable") == b"survives daemon restart"
                    break
                except (OSError, SMBResponseException):
                    smbclient.reset_connection_cache()
                    time.sleep(0.1)
            else:
                raise AssertionError("fresh SMB connection did not recover")
            self.env.write("after-restart", b"ok")
            assert self.env.read("after-restart") == b"ok"
            held = smbclient.open_file(
                self.env.path("shutdown-held"), "wb", buffering=0, **self.env.config
            )
            held.write(b"flushed-before-shutdown")
            held.flush()
            server.terminate()
            server.wait(timeout=10)
            try:
                held.write(b"must-fail")
            except (OSError, SMBResponseException):
                pass
            else:
                raise AssertionError("write succeeded after graceful backend shutdown")
            try:
                held.close()
            except (OSError, SMBResponseException):
                pass
        finally:
            try:
                smbclient.reset_connection_cache()
            except SMBResponseException:
                pass
            if server.poll() is None:
                server.terminate()
            server.wait(timeout=30)
            log.close()


def main():
    if len(sys.argv) == 2 and sys.argv[1] == "hold":
        env = Environment()
        try:
            with smbclient.open_file(
                env.path("abandoned"), "wb", buffering=0, share_access="rwd", **env.config
            ) as held:
                held.write(b"client-exit")
                print("READY", flush=True)
                time.sleep(120)
        finally:
            env.close()
        return 0
    # The final suite deliberately kills the daemon owned by run.sh.
    suite = unittest.TestSuite(
        unittest.defaultTestLoader.loadTestsFromTestCase(case) for case in (SMBTests, ShutdownTests)
    )
    return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
