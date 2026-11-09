"""Real SMB2/3 operations; requires smbprotocol (installed in integration CI)."""
import concurrent.futures
import errno
import json
import re
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import urllib.request

import smbclient
from smbprotocol.exceptions import SMBResponseException

PORT = int(os.environ["SMB_TEST_PORT"])
ROOT = Path(os.environ["SMB_TEST_ROOT"])
BASE = r"\\127.0.0.1\tier"
CONFIG = dict(username="root", password="tier-integration-test", port=PORT)
smbclient.register_session("127.0.0.1", **CONFIG)

if len(sys.argv) == 2 and sys.argv[1] == "hold":
    with smbclient.open_file(BASE + r"\abandoned", "wb", buffering=0, share_access="rwd", **CONFIG) as held:
        held.write(b"client-exit")
        print("READY", flush=True)
        time.sleep(120)
    raise SystemExit(0)


def path(name):
    return BASE + "\\" + name.replace("/", "\\")


def read(name, options=CONFIG):
    with smbclient.open_file(path(name), "rb", buffering=0, share_access="rwd", **options) as f:
        return f.read()


def write(name, data, options=CONFIG):
    with smbclient.open_file(path(name), "wb", buffering=0, **options) as f:
        offset = 0
        while offset < len(data):
            written = f.write(data[offset:])
            assert written > 0
            offset += written
        f.flush()


# Exercise the production shared-group identity, without forcing an SMB user to root.
user_options = dict(CONFIG, username="tier-test", connection_cache={})
try:
    write("ordinary-user", b"group-authorized", user_options)
    assert read("ordinary-user", user_options) == b"group-authorized"
    smbclient.mkdir(path("ordinary-user-directory"), **user_options)
    smbclient.remove(path("ordinary-user"), **user_options)
    smbclient.rmdir(path("ordinary-user-directory"), **user_options)
finally:
    smbclient.reset_connection_cache(connection_cache=user_options["connection_cache"])

# An explicit flush followed by more writes must use the same valid handle.
with smbclient.open_file(path("partial"), "w+b", buffering=0, **CONFIG) as f:
    f.write(b"abcdefgh")
    f.flush()
    f.seek(2)
    f.write(b"XY")
    f.flush()
    f.seek(0)
    assert f.read() == b"abXYefgh"
    f.truncate(5)
    f.flush()
assert read("partial") == b"abXYe"

# Cross-session stat and read observe writes; no mutable attribute TTL cache.
with smbclient.open_file(path("visible"), "wb", buffering=0, share_access="rwd", **CONFIG) as f:
    f.write(b"visible-before-close")
    assert smbclient.stat(path("visible"), **CONFIG).st_size == 20
    assert read("visible") == b"visible-before-close"

# Unicode names, directory paging and case-insensitive name lookup.
smbclient.mkdir(path("目录"), **CONFIG)
root_entries = {entry.name: entry for entry in smbclient.scandir(BASE, **CONFIG)}
assert root_entries["目录"].is_dir()
assert root_entries["partial"].is_file()
for i in range(300):
    write(f"目录/entry-{i:04}", str(i).encode())
assert set(smbclient.listdir(path("目录"), **CONFIG)) == {f"entry-{i:04}" for i in range(300)}
write("MixedCase", b"case")
assert read("mixedcase") == b"case"

# Distinct-file I/O can proceed concurrently across SMB sessions/processes.
def worker(index):
    data = bytes([index]) * (1024 * 1024 + 123)
    name = f"parallel-{index}"
    options = dict(CONFIG, connection_cache={})
    try:
        write(name, data, options)
        assert read(name, options) == data
    finally:
        smbclient.reset_connection_cache(connection_cache=options["connection_cache"])

with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
    list(pool.map(worker, range(8)))

# Several in-flight requests in one smbd connection exercise the tevent dispatcher.
def shared_worker(index):
    data = bytes([index]) * 32768
    name = f"shared-{index}"
    write(name, data)
    assert read(name) == data

with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
    list(pool.map(shared_worker, range(8)))

# Attributes preserve SMB's 100 ns precision; streams are explicitly unsupported.
stamp = 1_704_164_645_123_456_700
smbclient.utime(path("partial"), ns=(stamp, stamp), **CONFIG)
assert smbclient.stat(path("partial"), **CONFIG).st_mtime_ns == stamp
try:
    write("partial:private", b"unsupported stream")
except OSError:
    pass
else:
    raise AssertionError("named stream unexpectedly accepted")
assert read("partial") == b"abXYe"

# Samba retains share-mode enforcement even with kernel locking disabled.
with smbclient.open_file(path("exclusive"), "wb", buffering=0, **CONFIG) as exclusive:
    try:
        read("exclusive", dict(CONFIG, connection_cache={}))
    except OSError as error:
        assert error.ntstatus == 0xC0000043, error  # STATUS_SHARING_VIOLATION
    else:
        raise AssertionError("exclusive SMB open was bypassed")

# Abrupt client exit releases its writer in Tier, without restarting the daemon.
child = subprocess.Popen([sys.executable, __file__, "hold"], stdout=subprocess.PIPE, text=True)
try:
    assert child.stdout.readline().strip() == "READY"
finally:
    child.kill()
    child.wait(timeout=10)
for attempt in range(100):
    try:
        write("abandoned", b"reopened")
        break
    except OSError:
        if attempt == 99:
            raise
        time.sleep(0.1)
assert read("abandoned") == b"reopened"

# Namespace errors propagate; a failed mutation cannot silently succeed.
try:
    smbclient.rmdir(path("目录"), **CONFIG)
except OSError as error:
    assert error.errno == errno.ENOTEMPTY, error
else:
    raise AssertionError("nonempty directory removal succeeded")
smbclient.rename(path("partial"), path("renamed"), **CONFIG)
assert read("renamed") == b"abXYe"
try:
    smbclient.stat(path("partial"), **CONFIG)
except OSError as error:
    assert error.errno == errno.ENOENT, error
else:
    raise AssertionError("old rename path remains visible")

# Target identity loss must never redirect writes into a replacement mountpoint.
address = re.search(r"address=(127\.0\.0\.1:\d+)", (ROOT / "tier.log").read_text()).group(1)
admin = "http://" + address

def action(name, values):
    payload = json.dumps({"action": name, "input": values}).encode()
    request = urllib.request.Request(admin + "/api/action", data=payload,
                                     headers={"Content-Type": "application/json", "Origin": admin})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)

smbclient.mkdir(path("protected"), **CONFIG)
replica = ROOT / "replica"
replica.mkdir()
action("target", {"name": "replica", "path": str(replica), "allow_same_domain": True, "domain": "test-replica"})
policy = {"path": "/protected", "write_cache": ["main"], "persistent_storage": ["replica"],
          "persistent_copies": 1, "persist_delay_secs": 0, "direct_fallback": False,
          "low_water": 99, "high_water": 100}
action("policy", policy)
write("protected/saved", b"verified-long-term-data")
write("protected/stale", b"old-replica")
action("repair", {})
policy["persist_delay_secs"] = 3600
action("policy", policy)
write("protected/stale", b"latest-unreplicated-content")
primary = ROOT / "objects"
offline = ROOT / "objects-offline"
primary.rename(offline)
primary.mkdir()
try:
    assert read("protected/saved") == b"verified-long-term-data"
    try:
        read("protected/stale")
    except OSError:
        pass
    else:
        raise AssertionError("an obsolete replica was returned as current data")
    try:
        write("protected/no-target", b"must-fail")
    except OSError:
        pass
    else:
        raise AssertionError("write accepted without an eligible target")
    assert list(primary.iterdir()) == [], "replacement target was modified"
finally:
    primary.rmdir()
    offline.rename(primary)
assert read("protected/stale") == b"latest-unreplicated-content"
# Failed creation may leave an empty namespace entry, like a failed ordinary write.
try:
    smbclient.remove(path("protected/no-target"), **CONFIG)
except OSError as error:
    assert error.errno == errno.ENOENT
policy["persist_delay_secs"] = 0
action("policy", policy)
action("repair", {})
corrupt = [p for p in replica.rglob("*") if p.is_file() and p.read_bytes() == b"verified-long-term-data"]
assert len(corrupt) == 1
corrupt[0].write_bytes(b"corrupted-long-term-data")
action("repair", {})
assert any(p.is_file() and p.read_bytes() == b"verified-long-term-data" for p in replica.rglob("*"))
assert read("protected/saved") == b"verified-long-term-data"

# A killed backend invalidates old handles; a fresh tree connection recovers.
write("durable", b"survives daemon restart")
old = smbclient.open_file(path("durable"), "rb", buffering=0, share_access="rwd", **CONFIG)
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
log = (ROOT / "tier-restarted.log").open("wb")
server = subprocess.Popen(
    ["setpriv", "--regid", os.environ["TIER_TEST_GID"], "--clear-groups", os.environ["TIER_BIN"], "--db", str(ROOT / "metadata"), "serve", "--socket",
     str(ROOT / "tier.sock"), "--admin", "127.0.0.1:0"], stdout=log, stderr=subprocess.STDOUT,
)
try:
    for _ in range(100):
        try:
            smbclient.register_session("127.0.0.1", **CONFIG)
            assert read("durable") == b"survives daemon restart"
            break
        except (OSError, SMBResponseException):
            smbclient.reset_connection_cache()
            time.sleep(0.1)
    else:
        raise AssertionError("fresh SMB connection did not recover")
    write("after-restart", b"ok")
    assert read("after-restart") == b"ok"
    held = smbclient.open_file(path("shutdown-held"), "wb", buffering=0, **CONFIG)
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
print("PASS SMB partial write, flush, visibility, paging, concurrent I/O, timestamps, locks, client exit, target identity, stale replica rejection, replica repair, restart, graceful shutdown")
