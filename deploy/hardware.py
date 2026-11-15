#!/usr/bin/python3
"""Read-only SMART snapshot. Runs separately from the unprivileged Web server."""

import json
import os
import subprocess
import tempfile
import time
from pathlib import Path


def physical(device):
    device = device.resolve()
    if (device / "partition").exists():
        return physical(device.parent)
    slaves = list((device / "slaves").glob("*"))
    if slaves:
        return sorted({disk for slave in slaves for disk in physical(slave)})
    if not (device / "device").exists():
        return []
    return [device.name]


def normalize(data):
    attributes = {
        entry["id"]: entry.get("raw", {}).get("value")
        for entry in data.get("ata_smart_attributes", {}).get("table", [])
    }
    nvme = data.get("nvme_smart_health_information_log", {})
    return {
        "model": data.get("model_name") or data.get("product"),
        "serial": data.get("serial_number"),
        "temperature": data.get("temperature", {}).get("current"),
        "passed": data.get("smart_status", {}).get("passed"),
        "power_on_hours": data.get("power_on_time", {}).get("hours"),
        "percentage_used": nvme.get("percentage_used"),
        "reallocated": attributes.get(5),
        "pending": attributes.get(197),
        "uncorrectable": attributes.get(198),
        "media_errors": nvme.get("media_errors"),
    }


def read_disk(name):
    result = {"device": "/dev/" + name}
    try:
        command = subprocess.run(
            ["/usr/sbin/smartctl", "--all", "--json", result["device"]],
            capture_output=True,
            timeout=10,
            check=False,
        )
        data = json.loads(command.stdout)
        result.update(normalize(data))
        # smartctl uses a bitmask: health/error-log bits are valid readings.
        code = command.returncode
        result["warning"] = bool(code & 248)
        if code & 7:
            result["error"] = "SMART 读取不完整或设备不支持"
        if code & 8:
            result["passed"] = False
    except FileNotFoundError:
        result["error"] = "未安装 smartmontools"
    except subprocess.TimeoutExpired:
        result["error"] = "SMART 读取超时"
    except (OSError, ValueError):
        result["error"] = "无法读取 SMART 信息"
    return result


def collect():
    mapping = {}
    readings = {}
    for entry in sorted(Path("/sys/class/block").iterdir()):
        try:
            names = physical(entry)
            for name in names:
                if name not in readings:
                    readings[name] = read_disk(name)
            mapping[(entry / "dev").read_text().strip()] = [
                readings[name] for name in names
            ]
        except OSError:
            continue  # Device removed during collection; retry next round.
    return {"updated_at": int(time.time()), "devices": mapping}


def main():
    snapshot = collect()
    directory = Path("/run/tier-hardware")
    directory.mkdir(mode=0o755, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", dir=directory, delete=False) as file:
        temporary = Path(file.name)
        try:
            json.dump(snapshot, file)
            file.flush()
            os.fchmod(file.fileno(), 0o644)
            os.replace(temporary, directory / "status.json")
        finally:
            temporary.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
