# Tier

Tier 将 NAS 上的多个存储目录组织成一个 SMB 共享，通过 Web 控制台按目录配置**缓存、持久化存储、归档**，子目录默认继承父级策略。

文件按策略写入缓存盘或持久盘，后台再校验、补齐副本和归档。客户端要求同步落盘时，会同步当前文件及必要元数据，但不等待其他副本；副本补齐前，当前数据盘故障仍可能丢失最新内容。

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

先挂载数据盘，再在控制台登记各盘上的专用目录，配置根目录 `/` 的副本策略，并将元数据备份保存到不同设备。正文对象目录由 Tier 管理，不应直接修改。

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

文件映射导出为 CSV，记录逻辑路径和各副本的实际位置，用于找回文件或迁移到普通目录，不能用于恢复 Tier 元数据。正式迁移应先停服，再离线导出映射并复制文件，避免迁移期间映射变化：

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

示例中的 `disk1` 替换为控制台登记的设备名。确认检查报告后，将 TOML 的 `db` 指向恢复目录，再启动服务。恢复后两个保存计划默认停用，确认目的地后重新启用。只有明确接受不完整恢复时才使用 `--allow-degraded`。Tier 重启后，SMB 客户端需要重新连接。
