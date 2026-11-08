# Tier

Tier 为家庭 NAS 提供一个 SMB 共享目录。你可以让两块 SSD 同步保存每次写入，也可以先写一块 SSD，等文件安静一段时间后复制到机械盘，再将冷文件归档到 OpenList 等 WebDAV 服务。每个子目录可以继承或覆盖上级策略。

## 安装到 Linux 服务器

Tier 通过 Linux 内核的 FUSE 提供文件系统，Samba 将挂载点共享给 Windows、macOS 和其他 SMB 客户端。使用已编译的 Tier 无需安装 Rust、Node.js 或单独部署 Web 服务。

1. 准备支持 FUSE 的 Linux 主机。Debian 和 Ubuntu 可用 `sudo apt install fuse3 samba` 安装运行依赖。其他发行版用自己的包管理器安装 FUSE 3 和 Samba，确认存在 `/dev/fuse`。
2. 从 [GitHub Releases](https://github.com/RainMark/tier/releases/latest) 下载 `tier-linux-x86_64.tar.gz` 和 `SHA256SUMS`。当前预编译包面向 **x86-64、glibc**，在 Ubuntu 22.04 构建。ARM 和 musl 系统需要自行构建，发布包未覆盖这些平台。
3. 在下载目录校验并安装。包内安装器适用于使用 systemd 的主机。

```sh
sha256sum --check SHA256SUMS
tar -xzf tier-linux-x86_64.tar.gz
cd tier-linux-x86_64
sudo ./install.sh
sudo systemctl enable --now tier
systemctl status tier
```

如果使用其他服务管理器，可将包内的 `tier` 放入 PATH，按示例创建配置、数据库父目录和共享挂载目录，然后以前台命令 `tier --config /etc/tier/tier.toml serve` 交由服务管理器托管。

在 NAS 本机打开 `http://127.0.0.1:9480`。从自己的电脑管理时，先执行 `ssh -L 9480:127.0.0.1:9480 用户名@NAS地址`，再打开同一地址。日志用 `journalctl -u tier -f` 查看。默认级别为 info，需要调试时可通过 systemd 服务环境变量设置 `RUST_LOG=tier=debug`。

## 建立第一个共享

1. 将数据盘挂载到固定路径，在盘内建立 Tier 可写的目录。默认服务以 root 用户、tier 组访问本地磁盘。到控制台的「存储设备」登记这些目录，例如命名为 `ssd`、`disk1` 和 `disk2`。Tier 在登记时写入设备身份，拔盘后能识别目标离线。
2. 到「目录策略」配置根目录 `/`。双 SSD 可以把两块盘都选作前台目标，写入确认份数设为 2。少量 SSD 加机械盘可以只选 `ssd` 作前台目标，确认份数设为 1，再选 `disk1`、`disk2` 作长期目标，副本数设为 2、延迟设为 300 秒。延迟期间，刚写入的内容只有 SSD 上的一份。
3. 在「快照备份」选择位于不同设备上的备份目录和导出间隔。JSON 快照保存命名空间及副本清单；恢复文件还需要清单中的实际数据。如果启用云归档，只有远端当前对象通过读回校验，才允许释放最后一份本地数据。
4. 将包内 `samba-share.conf` 的共享段加入 Samba 配置，为已有 Linux 用户设置 SMB 密码（`sudo smbpasswd -a 用户名`），运行 `sudo testparm -s` 检查配置，然后启动或重载 Samba。Debian 和 Ubuntu 的服务名为 `smbd`，例如 `sudo systemctl enable --now smbd && sudo systemctl reload smbd`。Windows 访问 `\\NAS地址\tier`。保留共享段里的挂载检查和 `oplocks = no`。

## 配置与数据放在哪里

| 内容 | systemd 安装 | 手动运行 |
| --- | --- | --- |
| 启动配置 | `/etc/tier/tier.toml` | `~/.config/tier/tier.toml` |
| 元数据库 | `/var/lib/tier/tier.sqlite` | `~/.local/state/tier/tier.sqlite` |
| 共享挂载点 | `/srv/tier/share` | 由 TOML 或 `serve` 参数指定 |

手动运行遵循 `XDG_CONFIG_HOME` 和 `XDG_STATE_HOME`。TOML 定义 `db`、`mountpoint`、`admin` 三个启动参数，命令行显式参数优先，修改后重启服务。数据库旁的锁文件和 SQLite 辅助文件由 Tier 管理。文件内容存放在登记的数据设备或归档目标上。

设备、目录策略、快照计划和回收站由 Tier 存入同一个 SQLite 数据库。服务运行时通过 Web 修改，立即生效；停服维护时，CLI 可以对同一数据库操作，例如 `sudo tier --config /etc/tier/tier.toml status`。恢复时先用 `tier inspect snapshot.json /mnt/surviving-disk` 检查，再用 `tier --db /path/to/new.sqlite import snapshot.json --target disk1=/mnt/surviving-disk` 导入新库。旧版若使用工作目录下的 `tier.sqlite`，升级前将 TOML 的 `db` 指向原库绝对路径。更新发布包时重新运行 `install.sh`，已有 TOML 会保留；执行 `sudo systemctl restart tier` 切换到新版本。

## 从源码构建

在 Linux 上安装稳定版 Rust、C 编译器、GNU Make、Node.js 22.12+ 和 npm，运行 `make build`。生成的 `target/release/tier` 已包含控制台，在目标平台按上述方式部署。
