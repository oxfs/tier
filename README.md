# Tier

Tier 将家庭 NAS 上的多个存储目录组织成一个 SMB 共享。通过 Web 控制台按目录配置 **缓存、持久化存储、归档**，子目录默认继承最近一级策略。

前台始终写入一个目标文件：有缓存时优先写缓存，否则写入一块持久化存储。普通写入和关闭允许操作系统缓存；客户端显式 FLUSH 或 write-through 时，Tier 同步当前文件及必要元数据。关闭后，后台等待配置的延迟，再校验内容、补齐副本和执行归档。

| 配置 | 前台写入 | 后台保护 |
| --- | --- | --- |
| SSD 缓存 + 两块机械盘 | SSD 文件 | 在两块持久化存储上补齐副本 |
| 两块持久化存储，无缓存 | 其中一块盘 | 在另一块盘上补齐副本 |
| WebDAV 归档 | 本地目标 | 上传并读回校验后，按策略保留或释放本地副本 |

副本数是后台保护目标，关闭或 FLUSH 成功不表示副本已经补齐。补齐前主目标损坏可能丢失最新内容。写入中的文件不做复制、归档或缓存淘汰；旧副本不会被当作最新内容返回。普通读取直接读取本地文件，完整校验在后台保护、巡检和恢复时执行。

## 安装

支持 x86-64 Debian 13、systemd 和官方软件源中的 Samba。VFS 插件必须匹配发布包记录的 Samba 包版本。下载 `tier-debian13-amd64.tar.gz` 和 `SHA256SUMS` 到同一目录。发布包包含 Tier、Samba 插件和 Web 控制台。

```sh
sudo apt update
sudo apt install samba libprotobuf-c1
sha256sum --check SHA256SUMS
tar -xzf tier-debian13-amd64.tar.gz
cd tier-debian13-amd64
sudo ./install.sh
sudo systemctl enable --now tier
```

在 NAS 打开 `http://127.0.0.1:9480`。从其他电脑管理时，使用 `ssh -L 9480:127.0.0.1:9480 用户名@NAS地址`，再打开相同地址。

1. 在每块数据盘内创建 Tier 专用目录，通过 **存储设备** 登记。不要直接修改这些目录里的正文对象和身份文件。
2. 在 **副本策略** 配置根目录 `/` 的持久化存储及目标副本数，按需选择缓存、复制延迟、缓存水位和空间不足时的直写策略。
3. 在 **元数据** 选择不同设备上的快照目录，配置频率和保留规则。快照只包含元数据，正文仍在数据盘或归档目标上；保留旧快照不等于保留文件的所有历史正文。
4. 合并发布包 `samba-share.conf` 中的全局设置和 `[tier]` 共享段到 `/etc/samba/smb.conf`。共享的 `/` 是 Tier 虚拟根目录，不是主机根目录。不要删除 `vfs objects = tier`。

```sh
sudo smbpasswd -a 用户名
sudo testparm -s
sudo systemctl enable --now smbd
sudo systemctl restart smbd
```

Windows 打开 `\\NAS地址\tier`。服务通过本地 Unix socket 通信，不需要 FUSE 挂载。共享支持普通文件和目录，不提供命名流、扩展属性或 Apple fruit 扩展。同一文件仅允许一个写句柄，不同文件可以并行访问。

## 配置与恢复

| 内容 | systemd 安装 | 个人运行 |
| --- | --- | --- |
| 启动配置 | `/etc/tier/tier.toml` | `~/.config/tier/tier.toml` |
| RocksDB 元数据目录 | `/var/lib/tier/metadata` | `~/.local/state/tier/metadata` |
| Samba IPC socket | `/run/tier/tier.sock` | 元数据目录旁的 `metadata.sock` |

TOML 定义 `db`、`socket` 和 `admin`，修改后重启 Tier。设备、策略和快照计划通过控制台更新。个人运行遵循 `XDG_CONFIG_HOME`、`XDG_STATE_HOME`。日志用 `journalctl -u tier -f` 查看。

误删文件可从 **回收站** 恢复。元数据丢失时，先检查快照和存活设备，再导入新的元数据目录。例如将设备 `disk1` 映射到重新挂载的位置：

```sh
sudo tier inspect snapshot.json /mnt/surviving-disk
sudo tier --db /var/lib/tier/recovered import snapshot.json --target disk1=/mnt/surviving-disk
```

检查报告中的缺失、损坏、未确认写入、离线设备和归档错误。只有明确接受不完整恢复时才使用 `--allow-degraded`。检查无误后停服，将 TOML 的 `db` 指向新目录，再启动 Tier。Tier 重启会使原有 SMB 句柄失效，客户端需要重新连接。

更新时安装与当前 Samba 匹配的发布包，并重启 Tier 和 Samba。安装程序保留已有 TOML；Samba 的升级可能需要重新构建匹配插件。

## 源码构建

核心构建与测试在 Ubuntu 中运行，需要稳定版 Rust（含 rustfmt、Clippy、rust-analyzer）、C/C++ 编译器、Clang/libclang、clang-format、CMake、GNU Make、Node.js 22.12+ 和 npm。

```sh
make check
rust-analyzer diagnostics . --severity error
make build
```

Samba 插件在 Debian 13 中针对已安装的官方 Samba 源码包构建。开启 `deb-src`，安装 Samba 构建依赖及 `protobuf-c-compiler`、`libprotobuf-c-dev`，然后运行：

```sh
sudo apt build-dep samba
samba/build.sh /tmp/tier-samba-module
```

GitHub CI 会运行核心检查、容量故障测试，并在 Debian 13 构建插件、安装发布包、从真实 SMB 入口验证读写、并发、复制修复和进程重启。
