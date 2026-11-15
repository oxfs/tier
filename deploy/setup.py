#!/usr/bin/env python3
"""Initialize a fresh Linux Docker deployment without formatting disks."""

import argparse
import getpass
import hashlib
import json
import os
import platform
import re
import socket
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-pull", action="store_true")
    parser.add_argument("--primary")
    parser.add_argument("--replica")
    parser.add_argument("--password-file")
    parser.add_argument("--image", help="Explicit version tag or digest")
    parser.add_argument("--directory", default="/opt/tier")
    args = parser.parse_args()
    for key in list(os.environ):
        if key.startswith("COMPOSE_"):
            del os.environ[key]
    if (
        os.geteuid() != 0
        or platform.system() != "Linux"
        or platform.machine() != "x86_64"
    ):
        parser.error("Run as root on Linux amd64")
    if not args.image:
        version_file = Path(__file__).with_name("version")
        if not version_file.exists():
            parser.error("--image is required outside a release package")
        args.image = (
            "ghcr.io/oxfs/tier:" + version_file.read_text().strip().removeprefix("v")
        )
    if ":" not in args.image or args.image.endswith(":latest"):
        parser.error("Use an explicit image version or digest, not latest")
    destination = Path(args.directory).resolve()
    if destination.exists():
        parser.error(
            "Installation directory already exists; existing installations are never overwritten"
        )
    disks = [
        Path(value or input(prompt)).resolve()
        for value, prompt in [
            (args.primary, "Primary mounted disk: "),
            (args.replica, "Replica mounted disk: "),
        ]
    ]
    if (
        any(
            destination.is_relative_to(d) or d.is_relative_to(destination)
            for d in disks
        )
        or disks[0].is_relative_to(disks[1])
        or disks[1].is_relative_to(disks[0])
    ):
        parser.error("Installation and disk paths must not overlap")
    if not re.fullmatch(r"/[A-Za-z0-9_./-]+", str(destination)):
        parser.error("Use a simple absolute installation path")
    for disk in disks:
        if (
            disk.parent == Path("/")
            or not re.fullmatch(r"/[A-Za-z0-9_./-]+", str(disk))
            or not disk.is_mount()
        ):
            parser.error(f"Not a mounted data disk: {disk}")
        if any(disk.iterdir()):
            parser.error(f"Disk is not empty: {disk}")
    if disks[0].stat().st_dev == disks[1].stat().st_dev:
        parser.error("Primary and replica must be separate filesystems")
    ancestors = []
    for disk in disks:
        source = run(
            "findmnt",
            "-n",
            "-o",
            "SOURCE",
            "--mountpoint",
            str(disk),
            capture_output=True,
        ).stdout.strip()
        rows = run(
            "lsblk", "-s", "-n", "-r", "-o", "NAME,TYPE", source, capture_output=True
        ).stdout.splitlines()
        devices = {row.split()[0] for row in rows if row.split()[1] == "disk"}
        if not devices:
            parser.error(f"Cannot establish backing disk identity: {disk}")
        ancestors.append(devices)
    if ancestors[0] & ancestors[1]:
        parser.error("Primary and replica share a physical backing disk")
    for address, port in [("0.0.0.0", 445), ("127.0.0.1", 9999)]:
        with socket.socket() as listener:
            listener.bind((address, port))
    password = (
        Path(args.password_file).read_text().rstrip("\n")
        if args.password_file
        else getpass.getpass("SMB password: ")
    )
    if not password or "\n" in password or "\r" in password:
        parser.error("Password must be a nonempty single line")
    if not args.password_file and password != getpass.getpass("Confirm password: "):
        parser.error("Passwords differ")
    run("docker", "compose", "version")
    if not args.skip_pull:
        run("docker", "pull", args.image)
    uid = int(
        run(
            "docker",
            "run",
            "--rm",
            "--entrypoint",
            "id",
            args.image,
            "-u",
            "tier",
            capture_output=True,
        ).stdout
    )
    gid = int(
        run(
            "docker",
            "run",
            "--rm",
            "--entrypoint",
            "id",
            args.image,
            "-g",
            "tier",
            capture_output=True,
        ).stdout
    )
    for parent in dict.fromkeys(d.parent for d in disks):
        propagation = run(
            "findmnt", "-n", "-o", "PROPAGATION", "-T", str(parent), capture_output=True
        ).stdout.strip()
        if "shared" not in propagation.split(","):
            parser.error(
                f"Parent mount must already have persistent shared propagation: {parent}"
            )
    volumes = [
        {
            "type": "bind",
            "source": str(d.parent),
            "target": f"/mnt/{name}",
            "bind": {"create_host_path": False, "propagation": "rslave"},
        }
        for name, d in zip(("primary", "replica"), disks)
    ]
    env = dict(os.environ, TIER_DISKS=str(disks[0].parent), TIER_IMAGE=args.image)
    template_dir = Path(__file__).parent
    compose = json.loads(
        run(
            "docker",
            "compose",
            "-f",
            str(template_dir / "compose.yml"),
            "--profile",
            "*",
            "config",
            "--format",
            "json",
            "--no-path-resolution",
            env=env,
            capture_output=True,
        ).stdout
    )
    tier = compose["services"]["tier"]
    tier.pop("build", None)
    tier["image"] = args.image
    tier["volumes"] = volumes + [
        "./config:/etc/tier",
        "./smb.conf:/etc/samba/smb.conf:ro",
        "samba:/var/lib/samba",
        "cache:/var/cache/tier",
    ]
    tier["environment"]["TIER_SMB_ENABLED"] = "${TIER_SMB_ENABLED:-1}"
    for name in compose.get("volumes", {}):
        compose["volumes"][name] = {}
    compose["name"] = (
        "tier-" + hashlib.sha256(str(destination).encode()).hexdigest()[:12]
    )
    existing = run(
        "docker",
        "ps",
        "-aq",
        "--filter",
        "label=com.docker.compose.project=" + compose["name"],
        capture_output=True,
    ).stdout.strip()
    if existing:
        parser.error("Docker project already exists; refusing to replace it")
    for network in compose.get("networks", {}).values():
        network.pop("name", None)
    compose["secrets"]["smb_password"] = {"file": "./smb_password"}
    destination.mkdir(mode=0o700, parents=True)
    config = destination / "config"
    config.mkdir(mode=0o770)
    config.chmod(0o770)
    os.chown(config, uid, gid)
    paths = [
        f"/mnt/{name}/{disk.name}" for name, disk in zip(("primary", "replica"), disks)
    ]
    for disk in disks:
        os.chown(disk, uid, gid)
        os.chmod(disk, 0o770)
    (config / "config.toml").write_text(
        f'primary = {json.dumps(paths[0])}\n[replicas]\ndisk1 = {json.dumps(paths[1])}\n[[backup]]\npath = ""\ncount = 2\ngc = false\n'
    )
    os.chown(config / "config.toml", uid, gid)
    os.chmod(config / "config.toml", 0o640)
    template = Path(__file__).with_name("smb.conf.example").read_text()
    (destination / "smb.conf").write_text(
        template.replace("USERNAME", "tier").replace("/srv/tier/primary", paths[0])
    )
    secret = destination / "smb_password"
    secret.write_text(password + "\n")
    secret.chmod(0o600)
    (destination / "compose.yml").write_text(json.dumps(compose, indent=2) + "\n")
    run(
        "docker",
        "compose",
        "up",
        "-d",
        cwd=destination,
        env=dict(os.environ, TIER_SMB_ENABLED="0"),
    )
    for attempt in range(60):
        try:
            request = urllib.request.Request(
                "http://127.0.0.1:9999/api/init",
                data=b"{}",
                headers={"Content-Type": "application/json", "X-Tier-Request": "1"},
            )
            with urllib.request.urlopen(request, timeout=5) as response:
                result = json.load(response)
                if result.get("ok") is not True:
                    raise SystemExit(f"Initialization did not succeed: {result}")
            break
        except urllib.error.HTTPError as error:
            raise SystemExit(
                f"Initialization rejected: {error.read().decode(errors='replace')}"
            ) from error
        except OSError:
            if attempt == 59:
                raise SystemExit(
                    f"Initialization failed; inspect docker compose logs in {destination}. Files retained for recovery."
                )
            time.sleep(1)
    run(
        "docker",
        "compose",
        "up",
        "-d",
        cwd=destination,
        env=dict(os.environ, TIER_SMB_ENABLED="1"),
    )
    for attempt in range(60):
        try:
            with socket.create_connection(("127.0.0.1", 445), timeout=2):
                break
        except OSError:
            if attempt == 59:
                raise SystemExit("SMB did not start; inspect docker compose logs")
            time.sleep(1)
    print(
        f"Installed in {destination}. SMB user: tier. Forward localhost:9999 over SSH for management."
    )
    print(
        "Host disk mounts and shared propagation must be restored before Docker starts after reboot."
    )


if __name__ == "__main__":
    main()
