# Tier

Tier 把家庭 NAS 上的多个存储目录组织成一个 SMB 共享。电脑和手机通过同一个路径访问文件，管理员在 Web 控制台按目录决定写入何时确认、保留几份副本，以及何时将冷文件归档到 OpenList 等 WebDAV 服务。

| 使用方式 | 策略 | 写入何时确认 |
| --- | --- | --- |
| 两块 SSD 双写 | 写入缓存留空，两块 SSD 作为持久化存储，副本数设为 2 | 两份都写入后 |
| 一块 SSD 加两块机械盘 | SSD 作为写入缓存，缓存份数为 1，两块机械盘作为持久化存储，副本数为 2 | SSD 写入后。持久化完成前可能只有一份 |
| 冷文件归档 | 为目录选择 WebDAV 目标、归档延迟和本地保留份数 | 独立于写入确认。释放最后一份本地数据前会读回并校验远端文件 |

子目录默认继承最近一级策略。移动文件或修改策略后，Tier 在后台按新位置的策略调整副本。

## 安装

准备一台使用 systemd 的 x86-64 Debian 或 Ubuntu 主机，确认 `/dev/fuse` 可用。将发布包 `tier-linux-x86_64.tar.gz` 和 `SHA256SUMS` 下载到同一个目录。包内已包含 Web 控制台。[下载发布包](https://github.com/RainMark/tier/releases/latest)

```sh
sudo apt update
sudo apt install fuse3 samba
sha256sum --check SHA256SUMS
tar -xzf tier-linux-x86_64.tar.gz
cd tier-linux-x86_64
sudo ./install.sh
sudo systemctl enable --now tier
```

在 NAS 上打开 `http://127.0.0.1:9480`。从另一台电脑管理时，运行 `ssh -L 9480:127.0.0.1:9480 用户名@NAS地址`，保持 SSH 连接，再在电脑上打开同一地址。

## 建立共享

1. 将每块数据盘挂载到固定路径，在盘内创建专供 Tier 使用的目录。在 **存储设备** 中登记实际目录，给设备取名，例如 `ssd`、`disk1`、`disk2`。
2. 在 **副本策略** 配置共享根目录 `/`。按上表选择写入缓存和持久化存储。如果希望文件最后一次修改 5 分钟后再复制到机械盘，将持久化延迟设为 300 秒。使用缓存时还可设置淘汰水位，以及缓存满时拒写或直写持久化存储。
3. 在 **元数据** 选择不同设备上的快照目录。配置最近份数、保留全部快照的天数，或每小时、每日各保留一份。每次成功发布后按规则清理该目的地的过期快照。
4. 将发布包内 `samba-share.conf` 的 `[tier]` 段加入 `/etc/samba/smb.conf`。为已有 Linux 用户启用 SMB 登录，然后检查并加载配置。

```sh
sudo smbpasswd -a 用户名
sudo testparm -s
sudo systemctl enable --now smbd
sudo systemctl reload smbd
```

Windows 资源管理器打开 `\\NAS地址\tier`，使用上述账号登录。共享支持 Windows 文件属性和小型命名流，例如照片附带的 Apple 元数据。单个命名流连同 Samba 编码须在 64 KiB 以内。

## 配置与恢复

| 内容 | systemd 安装 | 个人运行 |
| --- | --- | --- |
| 启动配置 | `/etc/tier/tier.toml` | `~/.config/tier/tier.toml` |
| SQLite 元数据库 | `/var/lib/tier/tier.sqlite` | `~/.local/state/tier/tier.sqlite` |
| 共享挂载点 | `/srv/tier/share` | 由配置或 `serve` 参数指定 |

TOML 定义 `db`、`mountpoint` 和 `admin`，修改后重启 Tier。设备、目录策略和快照计划通过控制台修改，即时写入 SQLite。个人运行遵循 `XDG_CONFIG_HOME` 与 `XDG_STATE_HOME`。日志用 `journalctl -u tier -f` 查看。

误删文件可以先在 **回收站** 中恢复。若元数据库丢失，使用 JSON 快照及对应的数据盘或归档目标恢复。快照包含目录、副本清单和 SMB 命名流，文件正文仍位于存储目标上。以下示例将快照中的设备 `disk1` 映射到重新挂载的目录，并导入一个新数据库。

```sh
sudo tier inspect snapshot.json /mnt/surviving-disk
sudo tier --db /var/lib/tier/recovered.sqlite import snapshot.json --target disk1=/mnt/surviving-disk
```

检查无误后，停服，将 TOML 中的 `db` 指向新库，再启动服务。更新版本时重新运行发布包中的 `install.sh`，已有 TOML 会保留。同步 Samba 共享配置并重启 Tier。

从源码构建需要稳定版 Rust、C 编译器、GNU Make、Node.js 22.12+ 和 npm。运行 `make build` 即可生成含控制台的 `target/release/tier`。
