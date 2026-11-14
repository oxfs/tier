# Tier

Tier 通过 Samba 提供文件共享，后台按目录策略维护多盘副本和 WebDAV 归档。文件保留原目录和文件名；目录、描述信息和文件内容共同用于恢复，不需要数据库。

## 安装

### Debian 13

从 [Releases](https://github.com/oxfs/tier/releases) 下载适合机器架构的安装包。Samba 模块必须与安装包记录的 Debian Samba 版本一致；安装脚本会检查版本。

```sh
sudo apt update
sudo apt install samba
tar -xzf tier-debian13-amd64.tar.gz
sudo bash tier/deploy/install.sh
sudo install -m 0640 -o tier -g tier tier/deploy/config.toml.example /etc/tier/config.toml
```

先挂载各存储盘，再修改 `/etc/tier/config.toml` 中的路径和 `backup` 规则。`count` 是需要保留的内容总份数：包含未 GC 的主盘内容，以及配置的 WebDAV 归档份数。比如 `count = 2`，不 GC 时是主盘加另一份；开启 `gc = true` 后，主盘内容删除前必须在其他位置保留两份。目录和描述信息不计入内容份数。

让 `tier` 用户能够读写各存储目录，然后启动服务，在 Web 管理页面的「存储维护」中初始化磁盘身份：

```sh
sudo systemctl enable --now tier
```

参照安装包的 `deploy/smb.conf.example` 配置 `/etc/samba/smb.conf`，使共享路径与主盘一致，将 `USERNAME` 替换为允许访问共享的本机用户，并设置其 Samba 密码：

```sh
sudo smbpasswd -a USERNAME
sudo testparm
sudo systemctl enable --now tier
sudo systemctl restart smbd
```

## 使用

### Web 管理

服务默认在 `127.0.0.1:8739` 提供管理页面，可查看本地磁盘身份、容量和最近后台任务结果，并直接编辑 TOML 配置。远程管理可使用 SSH 转发：`ssh -L 8739:127.0.0.1:8739 USER@SERVER`，然后打开 `http://127.0.0.1:8739`。管理入口仅监听回环地址，不提供账号登录；如需共享远程访问，请使用带认证的反向代理。

保存前自动校验，保留注释，并检查配置是否已被外部修改。保存采用原子替换，`tier` 用户需要配置所在目录的写权限；安装脚本会设置该权限。后台在下一轮扫描读取配置；Samba 使用的配置变更需要重启 `smbd`。新增磁盘仍需显式初始化。页面展示最近运行结果，不代替完整内容校验；WebDAV 连通性以实际任务结果为准。

可通过 systemd 环境变量 `TIER_WEB_LISTEN` 修改本地监听地址，`TIER_WEB_ASSETS` 修改前端资源目录。直接运行 `tier CONFIG` 时设置 `TIER_WEB_LISTEN=127.0.0.1:8739` 启用管理页面。

### 文件共享

通过 SMB 访问文件。不要直接修改存储盘中的文件或 `.tier` 内部目录。后台自动补齐闲置文件的副本，完成后按策略释放主盘内容；配置变更在下一轮扫描生效。日志可通过 `journalctl -u tier` 查看。

已有本地副本的文件修改前会恢复到主盘。仍被访问的文件不能重命名或删除；恢复写入也需要等待旧副本读者关闭。归档文件只读，读取时从 WebDAV 下载并校验；目录浏览不会下载文件内容。

WebDAV 配置示例（最后修改满 7 天后归档，归档成功并校验后保留本地内容 30 天）：

使用归档策略必须配置至少一块独立的本地副本盘，用于保存目录和描述信息，即使最终文件内容全部放在 WebDAV。远端只保存按内容哈希寻址的文件内容，不保存文件名与目录映射；仅凭 WebDAV 内容无法恢复原目录结构。请勿将主盘与副本目录放在同一块物理盘上。

```toml
[archives.remote]
url = "https://dav.example.com/tier/"
username_env = "WEBDAV_USER"
password_env = "WEBDAV_PASSWORD"

[[backup]]
path = "archive"
count = 1
gc = true
archive = { count = 1, after_secs = 604800, local_ttl_secs = 2592000 }
```

归档前先在本地补齐总份数。Tier 自动选择本地副本盘与 WebDAV 目标；归档未完成、校验失败或份数不足时不会释放本地内容。`background.workers` 控制后台文件任务并发数，`background.archive_concurrency` 控制全局远端请求并发数，各 `[archives.NAME]` 的 `concurrency` 限制该目标请求并发数。

远端目录需要预先创建。凭据写入 `/etc/tier/secrets`，格式为 `WEBDAV_USER=...`、`WEBDAV_PASSWORD=...`；该文件仅允许管理员读取。Samba 进程也需要这些环境变量才能读取归档，可通过 `systemctl edit smbd` 为其配置同一个 `EnvironmentFile`。凭据变更后重启 `tier` 和 `smbd`。

## 恢复

遇到副本不一致，先保留各盘内容并查看日志，不要删除描述文件。本地盘缺失或身份不匹配时拒绝新的共享操作，需要人工处理；管理页面仍可查看故障。WebDAV 不可用不停止健康本地文件访问，但没有可用本地副本的归档文件可能无法读取。

跨盘重命名或删除中断时，恢复相关磁盘后重启服务，系统会重放已记录的操作。不要手动删除 `.tier/namespace.json`。

主盘故障时先停止 `smbd`，保护剩余盘，保留 Tier 运行以使用 Web 管理。其他盘保留的目录、描述信息及内容用于恢复；系统不会自动选择新主盘。尚未复制的新写入可能丢失。不要对缺失挂载点重新执行初始化。

恢复主盘时，停止 SMB 访问，将配置的 `primary` 指向一个已挂载的空目录，保留其他副本路径，在 Web「存储维护」中选择「恢复主盘」。恢复要求所有配置的本地副本描述信息一致，并逐个校验、取回文件内容；需要能容纳全部恢复内容的空间。完成后检查数据，再恢复共享。失败时保留目标目录用于检查，重新尝试须使用空目录。更换副本盘时，在同一页面选择对应磁盘的「重建」。

主盘恢复只能重建副本共同记录的最近状态，无法找回尚未复制的写入，也无法证明丢失主盘上的操作已经全部同步。若描述信息冲突，恢复被拒绝，需要人工核对。恢复期间必须保持 `smbd` 停止。
