# Tier

Tier 为家庭 SMB 共享提供统一目录和可配置的分层存储。Rust 服务维护 SQLite 命名空间，把普通文件放到登记的本地目录，并可归档到 WebDAV 集合（例如 OpenList 的 WebDAV 入口）。Samba 通过 FUSE 挂载点访问 Tier。

本机已构建的 Linux 可执行文件位于 `target/release/tier`。在 Ubuntu 进入 `/mnt/d/tier` 后，可运行 `./target/release/tier --help`。

## 构建

在 Linux 中安装稳定版 Rust、C 编译器、FUSE 3 和 Samba。WSL2 下可以直接在 Windows 编辑仓库，在 Ubuntu 构建。数据库、挂载点、临时文件和实际存储目标建议使用 Linux 文件系统。

```sh
cd /mnt/d/tier
export CARGO_TARGET_DIR="$HOME/.cache/tier/build"
cargo build --release --locked
tier="$CARGO_TARGET_DIR/release/tier"
```

## 建立第一个共享

下面以两个新目录模拟暂存和长期目标。真实设备应分别挂载后再登记。默认识别文件系统和可解析的 Linux 块设备父盘；复杂 RAID、虚拟磁盘或网络挂载可用 `--domain 名称` 显式指定故障域。`--allow-same-device` 用于明确接受同盘风险。

```sh
base="$HOME/tier-data"
mkdir -p "$base"/{cache,disk,share,snapshots}
db="$base/tier.sqlite"
"$tier" --db "$db" target cache "$base/cache"
"$tier" --db "$db" target disk "$base/disk" --allow-same-device
"$tier" --db "$db" policy / '{"front":["cache"],"front_count":1,"long":["disk"],"long_count":1,"delay_secs":30}'
"$tier" --db "$db" schedule "{\"directories\":[\"$base/snapshots\"],\"seconds\":30,\"changes\":100,\"keep\":20}"
"$tier" --db "$db" serve "$base/share"
```

服务运行期间，在本机打开 `http://127.0.0.1:9480` 管理目标、策略、修复、回收站和快照。挂载使用 `allow_other`；非 root 运行时需在 `/etc/fuse.conf` 中启用 `user_allow_other`。将 `deploy/samba-share.conf` 的路径改为实际挂载点，添加到 Samba 配置，设置 Samba 用户并确保其 Unix 身份能访问挂载点。先用 `testparm` 检查；首次部署用 `sudo systemctl start smbd` 启动，正在运行的服务可重新加载配置。Windows 访问 `\\服务器地址\tier`。部署时应保证挂载成功后才开放共享，退出前先断开共享，避免访问空挂载点。

## 放置和确认

| 字段 | 含义 |
|---|---|
| `front` / `front_count` | 可用于确认写入的目标和所需份数。双写 SSD 可将两个目标同时加入，份数设为 2。 |
| `long` / `long_count` | 长期目标候选及所需份数。文件在不同候选目标间分布。 |
| `delay_secs` | 最后一次修改后，允许开始长期复制的最短空闲时间。 |
| `direct_fallback` | 前台放置失败时尝试长期目标，按长期份数确认。 |
| `low_water` / `high_water` | 已用容量百分比。默认 70 / 90，后台和前台分别按水位回收有其他长期副本的暂存对象。 |
| `repair_grace_secs` | 设备离线后，自动修复前等待的秒数；管理员仍可主动修复。 |
| `manual_repair` | 已达到目标后发生副本损失时等待手动修复。初次放置仍自动进行。 |
| `trash_ttl_secs` | 删除条目的保留时间，默认 7 天。 |

根目录配置默认策略，显式配置的子目录整体覆盖继承策略。目录移动后按新位置核对。一个文件只允许一个可写句柄；同步写和 flush 按配置确认。失败或服务退出时遗留的写入标为未确认，管理者可删除后重传。完整本地副本位于目标的 `objects/`，盘根的 `.tier-id` 用于核对设备身份。独立副本的实际容错能力取决于它们所在的物理设备。

## 归档

登记已存在、可写、可读回的 WebDAV 集合。OpenList 的存储 WebDAV 策略请选择本地代理，确保 GET 直接返回文件内容；账户需要 WebDAV 访问、上传和删除权限。需要认证时，把用户名和密码放入服务环境，配置中只引用变量名。[OpenList WebDAV 文档](https://doc.oplist.org/guide/advanced/webdav)示例在停服时执行；运行中可使用管理界面配置。

```sh
"$tier" --db "$db" archive cloud https://nas.example/dav/tier --user-env TIER_DAV_USER --pass-env TIER_DAV_PASSWORD
"$tier" --db "$db" archive-policy / '{"targets":["cloud"],"copies":1,"delay_secs":7776000,"retain_local":1}'
```

`retain_local: 0` 表示允许删除最后一个本地副本。Tier 在发布及释放前验证远端对象内容，远端不可用时保留尚未回收的本地数据。仅有远端副本的文件需要联网读回。归档策略按目录继承，独立于本地策略。远端读取、上传和校验按块传输；挂载层和后台复制使用临时文件。请为 Ubuntu 的 `TMPDIR` 预留并发打开和复制文件的空间。

## 恢复

快照以带校验和的 UTF-8 JSON 保存。正常停机或进程重启使用原数据库；元数据设备损坏时可恢复至最近成功导出的位置。快照描述当时的目录，对已被覆盖或清理的数据不承担保留历史内容的作用。

```sh
"$tier" inspect snapshot.json /mnt/surviving-disk
"$tier" inspect snapshot.json /mnt/surviving-disk --extract /tmp/new-extraction
"$tier" --db /srv/recovered/tier.sqlite import snapshot.json --target disk=/mnt/surviving-disk
```

检查报告列出可验证、缺失、摘要不符和快照未引用的对象。提取要求新的目的目录；导入要求新的数据库，并保留稳定 ID。脱离 Tier 时仅只读访问受管盘。

## 验证

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
sudo env TIER_BIN="$CARGO_TARGET_DIR/release/tier" bash tests/smb-smoke.sh
```

SMB 冒烟测试在临时目录和非标准端口启动独立 Samba，覆盖客户端上传、重命名、下载、删除，以及挂载点的句柄与替换行为。
