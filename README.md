# Tier

Tier 通过 Samba 提供文件共享，后台按目录策略维护多盘副本和 WebDAV 归档。文件保留原目录和文件名；目录、描述信息和文件内容共同用于恢复，不需要数据库。

## 安装

### Debian 13

当前安装包支持 Debian 13 的 amd64（x86-64）机器。从 [Releases](https://github.com/oxfs/tier/releases) 下载安装包；Samba 必须与包内 `deploy/samba-version` 记录的 Debian 软件包版本一致，安装脚本会检查版本。

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

### Docker（Linux）

容器包含配套的 Samba 和 Tier。宿主机负责挂载磁盘；暂不支持容器内格式化、挂载和硬盘电源管理，也不采集 SMART。445 端口须未被其他 SMB 服务占用。

解压发布包后进入 `tier/deploy`。创建 `config` 目录，将 `config.toml.example` 复制为 `config/config.toml`，将 `smb.conf.example` 复制为 `smb.conf`，把其中的 `USERNAME` 替换为 `tier`。将 SMB 初始密码写入当前目录 `smb_password`（权限 0600）；账号保存到持久卷，修改密码使用 `docker compose exec tier smbpasswd tier`。

将主盘、副本盘分别挂载为同一个父目录下的子挂载点，例如 `/mnt/tier/primary`、`/mnt/tier/disk1`。配置中的路径分别填写 `/srv/tier/primary`、`/srv/tier/disk1`，删除未使用的副本配置。每个根目录必须为挂载点；容器拒绝把普通空目录作为磁盘启动。构建镜像后，按下方步骤查询 `tier` 的 UID/GID，再赋予其对配置目录及存储目录的读写权限，不要递归修改已有数据的权限。

```sh
export TIER_DISKS=/mnt/tier
sudo mount --rbind "$TIER_DISKS" "$TIER_DISKS"
sudo mount --make-rshared "$TIER_DISKS"
docker compose build
docker compose run --rm --entrypoint id tier tier
```

父目录使用 shared 挂载，使新增和移除的子挂载传播到容器；以上挂载设置重启后失效，需在宿主机启动流程中先恢复磁盘挂载及传播设置，再启动容器。完成目录权限设置后运行 `docker compose up -d`。

通过 SSH 转发 `9999` 端口访问管理页面，显式初始化磁盘身份后使用 SMB。配置目录需要允许 `tier` 用户原子替换文件。管理端口仅发布到宿主机回环地址，不带登录认证。容器内临时下载位于持久卷 `/var/cache/tier`，需预留足够空间；可将该卷改为独立磁盘目录。

可选 OpenList 归档：运行 `docker compose --profile archive up -d`，通过 SSH 转发 `5244` 访问其独立管理页面，按 OpenList 提示设置管理员和存储。创建专用归档账号及远端目录，允许 WebDAV 读取和上传，并使用代理下载以避免返回重定向。Tier 归档地址使用 `http://openlist:5244/dav/你的归档目录/`，凭据放在 `config/secrets`（权限 0600，纯 WEBDAV_NAME=value 格式，不使用引号或 Shell 表达式）。修改凭据后重启 Tier 容器。不要把 Tier 主盘直接挂给 OpenList，也不要让归档目标指回 Tier 的 SMB 共享。

默认示例固定 OpenList 版本；其他后端须先验证上传、完整回读、故障与恢复，再开启本地 GC。停止服务用 `docker compose down`；不要添加 `-v`，该选项会删除账号、OpenList 配置等持久数据。升级前备份 `config`、`smb.conf` 和命名卷，停止共享后用新发布包重建镜像。若精确 Samba 版本已从软件源移除，构建会失败，应使用重新构建的配套发布包，不能强行替换 Samba 版本。

磁盘故障需要管理恢复时，先运行 `docker compose stop tier`，再运行 `TIER_SMB_ENABLED=0 docker compose up -d tier`。维护模式仅启动 Tier 后台和管理页面，不启动 SMB，不要求缺失盘通过挂载检查。不要对丢失挂载点初始化。恢复主盘前，先在宿主机挂载一个有足够空间的空盘，确保其在容器中可见，再修改配置路径并按下文恢复流程操作。确认磁盘身份、数据与挂载正常后，运行 `TIER_SMB_ENABLED=1 docker compose up -d tier` 恢复共享。

## 升级

升级前阅读对应版本的发布说明，确认配置和磁盘格式兼容。本次首次发布不提供旧开发版本的数据格式迁移；不要直接在旧开发数据上升级，也不要依靠替换旧二进制回滚磁盘格式。

停止客户端访问，执行 `sudo systemctl stop smbd tier`，备份 `/etc/tier` 和 Samba 配置，保留所有存储盘内容（包括 `.tier`）。将新安装包解压到单独目录，在该目录执行：

```sh
sudo bash tier/deploy/install.sh
sudo testparm
sudo systemctl start tier
```

升级无需复制示例配置或重新初始化磁盘。先在 Web 检查配置、磁盘身份和扫描结果，确认正常后执行 `sudo systemctl start smbd`，通过 SMB 检查已有文件读取和新文件写入。

系统更新若改变了 Samba 软件包版本，也需要安装与新版本匹配的 Tier 模块，或针对该版本重新构建。更新前停止共享，在匹配模块安装完成后再启动 `smbd`；没有匹配包时先保留原 Samba 版本。

## 使用

### Web 管理

安装后的服务默认在 `0.0.0.0:9999` 提供管理页面，可通过 `http://服务器IP:9999` 访问，可查看本地磁盘身份、容量和最近后台任务结果，并直接编辑 TOML 配置。远程管理可使用 SSH 转发：`ssh -L 9999:127.0.0.1:9999 USER@SERVER`，然后打开 `http://127.0.0.1:9999`。管理页面不提供账号登录，只应在可信内网开放。需要限制本机访问时，将 `TIER_WEB_LISTEN` 设为 `127.0.0.1:9999`；公网访问应使用带认证的反向代理。

保存前自动校验，保留注释，并检查配置是否已被外部修改。保存采用原子替换，`tier` 用户需要配置所在目录的写权限；安装脚本会设置该权限。副本份数、归档等待时间、本地保留期和回收策略在下一轮扫描生效，无需重启服务。磁盘路径等 Samba 连接使用的配置变更需要重启 `smbd`，新增磁盘仍需显式初始化。页面展示最近运行结果，不代替完整内容校验；WebDAV 连通性以实际任务结果为准。

可通过 systemd 环境变量 `TIER_WEB_LISTEN` 修改监听地址，`TIER_WEB_ASSETS` 修改前端资源目录。直接运行 `tier CONFIG` 时设置 `TIER_WEB_LISTEN=0.0.0.0:9999` 启用管理页面。

磁盘硬件信息由独立服务每两分钟采集一次，显示型号、序列号、温度、通电时间和设备支持的 SMART 健康／寿命指标。安装脚本会安装 `smartmontools` 并启用 `tier-hardware.timer`。USB 硬盘盒、硬件 RAID 或虚拟设备可能无法提供完整信息；读取失败不影响副本维护，超过十分钟未更新的数据会标记为过期。SMART 正常不保证数据完整，请结合后台校验结果判断。

建议关闭存储盘休眠，让存储盘持续在线，具体设置参见[关闭存储盘休眠](docs/disk-power.md)。硬件采集不跳过休眠盘，查询可能唤醒设备。

### 文件共享

通过 SMB 访问文件。不要直接修改存储盘中的文件或 `.tier` 内部目录。文件先写入主盘，后台再补齐闲置文件的副本、完成归档，并按策略释放主盘内容；写入成功不表示副本已经完成。配置变更在下一轮扫描生效，日志可通过 `journalctl -u tier` 查看。

新文件默认以主盘内容为主副本。后台转储后，描述信息记录主副本所在磁盘；之后直接在该位置读写，不必搬回主盘。修改后，其他旧副本等待后台更新，不再作为当前内容读取。本地读取不重复计算全文件哈希；内容由 Tier 管理，不要直接修改底层副本。

仍被访问的文件不能重命名或删除，后台也不会切换或回收它的主副本。归档文件只读，读取时从 WebDAV 完整下载并校验后才提供内容；目录浏览不会下载文件内容。

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

归档前先在本地补齐总份数。Tier 自动选择本地副本盘与 WebDAV 目标；归档未完成、校验失败或份数不足时不会释放本地内容。修改本地保留期后，后台使用原归档成功时间和新的保留期重新判断：缩短后可在下一轮回收，延长会保留尚未回收的内容，不会自动取回已经回收的归档。`background.workers` 控制后台文件任务并发数，`background.archive_concurrency` 控制全局远端请求并发数，各 `[archives.NAME]` 的 `concurrency` 限制该目标请求并发数。

远端目录需要预先创建。凭据写入 `/etc/tier/secrets`，格式为 `WEBDAV_USER=...`、`WEBDAV_PASSWORD=...`；该文件仅允许管理员读取。Samba 进程也需要这些环境变量才能读取归档，可通过 `systemctl edit smbd` 为其配置同一个 `EnvironmentFile`。凭据变更后重启 `tier` 和 `smbd`。

## 恢复

遇到副本不一致，先保留各盘内容并查看日志，不要删除描述文件。副本盘故障不阻止健康主盘文件的读写，相关副本任务会报错，未满足保护份数时不会释放原内容；跨盘重命名和删除需要相关磁盘可用。打开文件时检查所需存储位置，已打开的文件继续由操作系统处理读写和 I/O 错误。主盘不可用时不能打开新的文件；WebDAV 不可用时，没有本地内容的归档文件可能无法读取。

跨盘重命名或删除中断时，恢复相关磁盘后重启服务，系统会重放已记录的操作。不要手动删除 `.tier/namespace.json`。

主盘或副本盘故障时，先执行 `sudo systemctl stop smbd`，保留 Tier 运行以使用 Web 管理，并保护剩余盘。用 `findmnt -T /实际存储路径` 核对替换路径确实位于预期磁盘上，再确认目标目录为空且 `tier` 用户可写；不要对缺失挂载点重新初始化。

- **恢复主盘**：将配置的 `primary` 指向空白替换目录，保留其他副本路径，在 Web「存储维护」选择「恢复主盘」。所有配置的本地副本描述信息必须一致；恢复会逐个校验并取回内容，主盘空间须能容纳全部恢复内容。若主盘路径改变，同时修改 Samba 共享路径。
- **重建副本盘**：把空白替换盘挂载到原副本路径，在 Web「存储维护」选择对应磁盘的「重建」。

完成后检查 Web 磁盘状态，执行扫描并核对目录、文件数量及重要文件内容；运行 `sudo testparm`，确认无误后执行 `sudo systemctl start smbd` 恢复共享。恢复失败时保留目标目录用于检查，重新尝试须使用空目录。

主盘恢复只能重建副本共同记录的最近状态，无法找回尚未复制的写入，也无法证明丢失主盘上的操作已经全部同步。若描述信息冲突，恢复被拒绝，需要人工核对。恢复期间必须保持 `smbd` 停止。
