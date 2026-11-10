# Tier

Tier 将 NAS 上的多个存储目录组织成一个 SMB 共享，通过 Web 控制台按目录配置**缓存、持久化存储、归档**，子目录默认继承父级策略。

前台写入一块盘：优先写缓存，否则写持久化存储。文件关闭后，后台按策略校验内容、补齐副本并归档。普通读写直接访问目标文件；客户端 FLUSH 或 write-through 同步当前文件及必要元数据。副本补齐前主盘损坏可能丢失最新内容，FLUSH 成功不表示副本已补齐。

正文对象以 `UUID--文件名` 命名，逻辑重命名后由后台更新；过长的名称提示会截断，完整名称仍保存在元数据中。

## 安装

### Debian 13

使用 x86-64 和 Debian 官方源的 Samba。从 [Releases](https://github.com/RainMark/tier/releases) 下载 `tier-debian13-amd64.tar.gz` 与 `SHA256SUMS` 到同一目录；安装程序会检查架构和完整 Samba 包版本是否匹配。

```sh
sudo apt update
sudo apt install samba libprotobuf-c1
sha256sum --check SHA256SUMS
tar -xzf tier-debian13-amd64.tar.gz
cd tier-debian13-amd64
sudo ./install.sh
sudo systemctl enable --now tier
```

在 NAS 上打开 `http://127.0.0.1:9480`；远程管理可先运行 `ssh -L 9480:127.0.0.1:9480 用户名@NAS地址`。

在控制台登记各数据盘上的专用目录，配置根目录 `/` 的副本策略，并将元数据备份保存到不同设备。正文对象目录由 Tier 管理，不应直接修改。

将包内 `samba-share.conf` 的全局设置和共享配置合并到 `/etc/samba/smb.conf`。其中 `path = /` 表示 Tier 虚拟根目录。为已有 Linux 用户设置 SMB 密码并启动共享：

```sh
sudo smbpasswd -a 用户名
sudo testparm -s
sudo systemctl enable smbd
sudo systemctl restart smbd
```

Windows 通过 `\\NAS地址\tier` 访问。升级 Samba 后，安装匹配的 Tier 发布包并重启两个服务。

## 配置与恢复

启动配置为 `/etc/tier/tier.toml`，元数据默认存放在 `/var/lib/tier/metadata`。设备和策略通过控制台管理；「元数据」页分别配置**文件映射导出**和**元数据备份**的启用状态、间隔、保存目录及保留份数。后端日志用 `journalctl -u tier -f` 查看，插件日志位于 Samba 配置的日志目录。

TOML 的 `[rocksdb]` 可调整元数据缓存、写缓冲、后台任务数和压缩方式，默认值见包内 `tier.toml`，修改后重启 Tier 生效。

文件映射是两列 CSV：`logical_path,physical_path`，一个当前副本一行，不保证每个副本在线；缺少来源时导出报错。目录以末尾 `/` 标识，物理路径留空。它用于查找正文或迁移到普通目录，不用于恢复 Tier。正式迁移时，先停服，再离线导出最终映射并复制正文，避免后台改名或迁移使路径变化：

```sh
sudo systemctl stop tier
sudo tier --config /etc/tier/tier.toml export /mnt/backup/files.csv
```

误删文件可从回收站恢复。元数据备份保存完整数据库，不包含正文；元数据丢失时，使用备份和存活设备检查、恢复到新的目录。省略 `--backup-id` 时使用最新一份：

```sh
sudo tier backups /mnt/backup/metadata
sudo systemctl stop tier
sudo tier inspect /mnt/backup/metadata /mnt/surviving-disk
sudo tier --db /var/lib/tier/recovered restore /mnt/backup/metadata --target disk1=/mnt/surviving-disk
```

确认检查报告后，将 TOML 的 `db` 指向恢复目录，再启动服务。恢复后两个保存计划默认停用，确认目的地后重新启用。只有明确接受不完整恢复时才使用 `--allow-degraded`。Tier 重启后，SMB 客户端需要重新连接。

## 开发

在 Ubuntu 中构建和测试。需要稳定版 Rust（rustfmt、Clippy、rust-analyzer）、C/C++ 编译器、Clang/libclang、clang-format、CMake、Make、ShellCheck、shfmt、Python 3.12+ 和 Node.js 22.12+。

```sh
python3 -m venv /tmp/tier-dev
. /tmp/tier-dev/bin/activate
pip install -r tests/requirements-dev.txt
make check
rust-analyzer diagnostics . --severity error
make build
```

Samba 插件在 Debian 13 中构建。启用 `deb-src`，安装 `samba`、`samba-dev`、`protobuf-c-compiler`、`libprotobuf-c-dev` 和 `apt build-dep samba` 所需依赖，然后运行 `samba/build.sh /tmp/tier-module`。CI 会打包并在干净 Debian 环境中验证安装、真实 SMB 操作与故障恢复。
