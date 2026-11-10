"""Comparable single-copy SMB workloads; reports timings without a hardware-specific gate."""

import json
import os
import time
import uuid

import smbclient
from smbclient._pool import get_smb_tree
from smbprotocol.open import (
    CreateDisposition,
    CreateOptions,
    FileAttributes,
    FilePipePrinterAccessMask,
    ImpersonationLevel,
    Open,
    ShareAccess,
)


def main():
    options = dict(
        username="root", password="tier-integration-test", port=int(os.environ["SMB_TEST_PORT"])
    )
    count = int(os.environ.get("TIER_BENCH_FILES", "256"))
    data = b"tier" * 1024
    results = {}
    for share in ("native", "tier"):
        for stable in (False, True):
            directory = rf"\\127.0.0.1\{share}\benchmark-{uuid.uuid4().hex}"
            smbclient.mkdir(directory, **options)
            started = time.perf_counter()
            for index in range(count):
                tree, name = get_smb_tree(directory + rf"\file-{index:06}", **options)
                handle = Open(tree, name)
                handle.create(
                    ImpersonationLevel.Impersonation,
                    FilePipePrinterAccessMask.GENERIC_READ
                    | FilePipePrinterAccessMask.GENERIC_WRITE,
                    FileAttributes.FILE_ATTRIBUTE_NORMAL,
                    ShareAccess.FILE_SHARE_READ
                    | ShareAccess.FILE_SHARE_WRITE
                    | ShareAccess.FILE_SHARE_DELETE,
                    CreateDisposition.FILE_CREATE,
                    CreateOptions.FILE_NON_DIRECTORY_FILE,
                )
                try:
                    assert handle.write(data) == len(data)
                    if stable:
                        handle.flush()
                finally:
                    handle.close()
            elapsed = time.perf_counter() - started
            listed = time.perf_counter()
            assert len(smbclient.listdir(directory, **options)) == count
            listing = time.perf_counter() - listed
            results[f"{share}_{'flush' if stable else 'buffered'}"] = {
                "files": count,
                "file_bytes": len(data),
                "seconds": round(elapsed, 6),
                "files_per_second": round(count / elapsed, 2),
                "list_seconds": round(listing, 6),
            }
    smbclient.reset_connection_cache()
    encoded = json.dumps(results, indent=2)
    print(encoded)
    if output := os.environ.get("TIER_BENCH_OUTPUT"):
        with open(output, "w") as stream:
            stream.write(encoded + "\n")


if __name__ == "__main__":
    main()
