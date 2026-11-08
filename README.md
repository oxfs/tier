# Tier

Tier 是面向家庭 SMB 共享的分层存储服务。Rust 核心维护统一目录，并按目录策略决定写入确认份数、长期副本和 WebDAV 归档位置。Windows 通过 Samba 访问 FUSE 挂载点，管理员通过本机 Web 界面管理存储。

## 构建

需要 Linux、稳定版 Rust、C 编译器、GNU Make、Node.js 22.12+ 和 npm。运行服务还需要 FUSE 和 Samba。

```sh
make build
./target/release/tier --help
```

前端由 Vue、TypeScript 和 Vite 构建，产物嵌入 Rust 二进制。部署主机只需运行 `tier`。设置了 `CARGO_TARGET_DIR` 时，可执行文件位于该目录的 `release/` 下。

## 建立共享

以下示例用本机两个目录演示“暂存确认一份，安静 30 秒后复制到长期盘”。真实设备应分别挂载再登记。示例中的 `--allow-same-device` 明确接受两个目标位于同一文件系统。

```sh
tier="${CARGO_TARGET_DIR:-target}/release/tier"
base="$HOME/tier-data"
mkdir -p "$base"/{cache,disk,share,backups}
db="$base/tier.sqlite"
"$tier" --db "$db" target cache "$base/cache"
"$tier" --db "$db" target disk "$base/disk" --allow-same-device
"$tier" --db "$db" policy / '{"front":["cache"],"front_count":1,"long":["disk"],"long_count":1,"delay_secs":30}'
"$tier" --db "$db" serve "$base/share"
```

在 `http://127.0.0.1:9480` 管理目录策略、设备、修复、回收站和快照。非 root 挂载需要在 `/etc/fuse.conf` 启用 `user_allow_other`。将 [Samba 共享配置](deploy/samba-share.conf) 中的路径替换为挂载点，配置可访问该目录的 Samba 用户，用 `testparm` 检查后启动或重新加载 `smbd`。保留 `oplocks = no`，确保 Windows 客户端遵守单写者规则。

## 数据保证

- **写入**。flush、同步写及最终关闭按目录策略确认。`front_count: 1` 表示长期复制完成前可能只有一份。暂存达到高水位时，只回收已有足够健康副本的文件；可配置直接失败或按 `fallback_count` 直写长期池。
- **放置**。子目录继承最近的显式策略。移动文件或改变策略会触发重新放置。目标身份及故障域用于避免误写卸载后的空目录，并优先将副本放到不同设备。
- **归档**。OpenList 需要可读写、可删除且能直接返回内容的 WebDAV 集合。只有策略允许、远端当前对象读回校验成功时，才释放最后一份本地数据。为 `TMPDIR` 预留打开、复制和召回文件的空间。
- **恢复**。多目标 JSON 快照保存目录和当前副本清单。它不保留被覆盖的历史文件。导入会预检设备、回收站和远端对象；遇到缺失时先审阅报告，接受后再用 `--allow-degraded`。

```sh
"$tier" inspect snapshot.json /mnt/surviving-disk
"$tier" inspect snapshot.json /mnt/surviving-disk --extract /tmp/tier-recovered
"$tier" --db /srv/tier/recovered.sqlite import snapshot.json --target disk=/mnt/surviving-disk
```

## 开发与验证

`make check` 构建前端并运行 rustfmt、严格 Clippy 和 Rust 测试。SMB 验收运行 `sudo env TIER_BIN="$tier" bash tests/smb-smoke.sh`。设备卸载及满盘测试分别位于 `tests/device-smoke.sh`、`tests/capacity-smoke.sh`；真实 Windows 客户端使用 `tests/windows-smb.ps1`。
