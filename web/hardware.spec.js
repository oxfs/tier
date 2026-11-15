import { test, expect } from "@playwright/test";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawn } from "node:child_process";

// Run in a private mount namespace: sudo unshare --mount --propagation private ...
test("collector snapshot reaches the real HTTP API and disk card", async ({
  page,
}) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tier-hardware-"));
  const binary = path.resolve(process.env.TIER_BIN ?? "../target/debug/tier");
  const mount = (...args) => execFileSync("mount", args);
  let server;
  try {
    for (const dir of ["primary", "sys/sda/device", "bin", "run"])
      fs.mkdirSync(path.join(root, dir), { recursive: true });
    const dev = BigInt(fs.statSync(root).dev);
    const major = ((dev >> 8n) & 0xfffn) | ((dev >> 32n) & 0xfffff000n);
    const minor = (dev & 0xffn) | ((dev >> 12n) & 0xffffff00n);
    fs.writeFileSync(path.join(root, "sys/sda/dev"), `${major}:${minor}`);
    mount("--bind", path.join(root, "sys"), "/sys/class/block");
    mount("--bind", path.join(root, "bin"), "/usr/sbin");
    mount("--bind", path.join(root, "run"), "/run");
    fs.mkdirSync("/run/tier-hardware");
    const config = path.join(root, "config.toml");
    fs.writeFileSync(
      config,
      `primary=${JSON.stringify(path.join(root, "primary"))}\n[replicas]\n[[backup]]\npath=''\ncount=1\n`,
    );
    const smart = (data, code = 0) =>
      fs.writeFileSync(
        "/usr/sbin/smartctl",
        `#!/bin/sh\nprintf '%s' '${JSON.stringify(data)}'\nexit ${code}\n`,
        { mode: 0o755 },
      );
    smart({
      model_name: "E2E disk",
      serial_number: "E2E-123",
      temperature: { current: 38 },
      smart_status: { passed: true },
      power_on_time: { hours: 123 },
    });
    execFileSync(binary, ["--hardware"]);
    server = spawn(binary, [config], {
      env: {
        ...process.env,
        TIER_WEB_LISTEN: "127.0.0.1:18739",
        TIER_WEB_ASSETS: path.resolve("dist"),
      },
      stdio: "ignore",
    });
    await expect
      .poll(async () => {
        try {
          return (
            await fetch("http://127.0.0.1:18739/api/health", {
              headers: { "X-Tier-Request": "1" },
            })
          ).status;
        } catch {
          return 0;
        }
      })
      .toBe(200);
    const initialized = await fetch("http://127.0.0.1:18739/api/init", {
      method: "POST",
      headers: { "X-Tier-Request": "1", "Content-Type": "application/json" },
      body: JSON.stringify({ confirmed: true }),
    });
    expect(initialized.ok).toBe(true);
    await page.goto("http://127.0.0.1:18739");
    await expect(page.getByText("E2E disk", { exact: true })).toBeVisible();
    await expect(page.getByText(/温度：38°C/)).toBeVisible();
    smart({ model_name: "E2E disk", smart_status: { passed: false } }, 8);
    execFileSync(binary, ["--hardware"]);
    await page.reload();
    await expect(page.getByText("SMART：异常", { exact: true })).toBeVisible();
    smart({}, 2);
    execFileSync(binary, ["--hardware"]);
    await page.reload();
    await expect(page.getByText(/SMART 读取不完整或设备不支持/)).toBeVisible();
    const cache = "/run/tier-hardware/status.json";
    const stale = JSON.parse(fs.readFileSync(cache));
    stale.updated_at = 1;
    fs.writeFileSync(cache, JSON.stringify(stale));
    await page.reload();
    await expect(page.getByText(/已过期/)).toBeVisible();
    fs.unlinkSync(cache);
    await page.reload();
    await expect(page.getByText(/尚未采集硬件信息/)).toBeVisible();
  } finally {
    if (server) {
      server.kill("SIGTERM");
      await new Promise((resolve) => server.once("exit", resolve));
    }
    for (const target of ["/run", "/usr/sbin", "/sys/class/block"])
      execFileSync("umount", [target]);
    fs.rmSync(root, { recursive: true, force: true });
  }
});
