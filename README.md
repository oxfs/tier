# Tier

Tier 通过 Samba 提供文件共享，后台按目录策略维护多盘副本和 WebDAV 归档。文件保留原目录和文件名；目录、描述信息和文件内容共同用于恢复，不需要数据库。

## 安装

### Debian 13

从 [Releases](https://github.com/RainMark/tier/releases) 下载适合机器架构的安装包。Samba 模块必须与安装包记录的 Debian Samba 版本一致；安装脚本会检查版本。

```sh
sudo apt update
sudo apt install samba
tar -xzf tier-debian13-amd64.tar.gz
sudo bash tier/deploy/install.sh
sudo install -m 0640 -o root -g tier tier/deploy/config.toml.example /etc/tier/config.toml
```

先挂载各存储盘，再修改 `/etc/tier/config.toml` 中的路径和 `backup` 规则。`count` 是需要保留的内容总份数：包含未 GC 的主盘内容，以及配置的 WebDAV 归档份数。比如 `count = 2`，不 GC 时是主盘加另一份；开启 `gc = true` 后，主盘内容删除前必须在其他位置保留两份。目录和描述信息不计入内容份数。

让 `tier` 用户能够读写各存储目录，然后初始化磁盘身份：

```sh
sudo runuser -u tier -- tier /etc/tier/config.toml init
```

参照安装包的 `deploy/smb.conf.example` 配置 `/etc/samba/smb.conf`，使共享路径与主盘一致，将 `USERNAME` 替换为允许访问共享的本机用户，并设置其 Samba 密码：

```sh
sudo smbpasswd -a USERNAME
sudo testparm
sudo systemctl enable --now tier
sudo systemctl restart smbd
```

## 使用

通过 SMB 访问文件。不要直接修改存储盘中的文件或 `.tier` 内部目录。后台自动补齐闲置文件的副本，完成后按策略释放主盘内容；配置变更在下一轮扫描生效。日志可通过 `journalctl -u tier` 查看。

已有本地副本的文件修改前会恢复到主盘。仍被访问的文件不能重命名或删除；恢复写入也需要等待旧副本读者关闭。归档文件只读，读取时从 WebDAV 下载并校验；目录浏览不会下载文件内容。

WebDAV 配置示例：

```toml
[archives.remote]
url = "https://dav.example.com/tier/"
username_env = "WEBDAV_USER"
password_env = "WEBDAV_PASSWORD"

[[backup]]
path = "archive"
count = 1
gc = true
archive = { targets = ["remote"], count = 1 }
```

远端目录需要预先创建。凭据写入 `/etc/tier/secrets`，格式为 `WEBDAV_USER=...`、`WEBDAV_PASSWORD=...`；该文件仅允许管理员读取。Samba 进程也需要这些环境变量才能读取归档，可通过 `systemctl edit smbd` 为其配置同一个 `EnvironmentFile`。凭据变更后重启 `tier` 和 `smbd`。

## 恢复

遇到副本不一致，先保留各盘内容并查看日志，不要删除描述文件。单个副本盘故障不影响主盘上仍存在的文件；后台会报告未满足的策略。

跨盘重命名或删除中断时，恢复相关磁盘后重启服务，系统会重放已记录的操作。不要手动删除 `.tier/namespace.json`。

主盘故障时先停止 `smbd` 和 `tier`，保护剩余盘。其他盘保留的目录、描述信息及内容用于恢复；系统不会自动选择新主盘。尚未复制的新写入可能丢失。不要对缺失挂载点重新执行初始化。
