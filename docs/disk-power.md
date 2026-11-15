# 关闭存储盘休眠（Debian 13）

常开 NAS 如果希望避免硬盘唤醒延迟，可以按盘关闭省电。Tier 不管理硬盘电源；这些设置属于操作系统，会增加空闲功耗，不能修复供电、线材或硬盘故障。SSD 没有电机停转，不必照搬机械盘参数。

## 确认设备

```sh
sudo apt install hdparm
lsblk -d -o NAME,MODEL,SERIAL,TRAN,ROTA
ls -l /dev/disk/by-id/
```

下面的设备名、序列号、控制器地址都必须替换为本机值。选择整盘的 `by-id` 路径，不选 `-part1` 等分区；不要把可能随重启改变的 `/dev/sdX` 写进配置。修改前备份已有配置。

## SATA 机械盘：关闭 APM 和停转计时器

```sh
disk=/dev/disk/by-id/ata-MODEL_SERIAL
sudo hdparm -B 255 -S 0 "$disk"
sudo hdparm -B "$disk"
```

`-B 255` 关闭 APM；`-S 0` 禁用自动停转计时器。部分盘不支持关闭 APM，若报错或读回未关闭，单独尝试 `sudo hdparm -B 254 "$disk"`，读回确认后使用 `254`。不要因为命令打印了“setting”就认定成功。

在 `/etc/hdparm.conf` 添加该盘的配置块（已有同盘配置时修改原块）：

```text
/dev/disk/by-id/ata-MODEL_SERIAL {
    apm = 255
    apm_battery = 255
    spindown_time = 0
}
```

如实际只能使用 `254`，同步替换两个 APM 值。Debian 13 的 hdparm 包通过 udev 在设备出现时应用配置；上面的手动命令用于立即生效。没有 APM 支持的设备不要硬套此配置，应查设备文档。

SSD 可以保持 `254`（APM 性能优先档），也可在实测支持时设为 `255`。SSD 配置块只写 `apm` 和 `apm_battery`，不需要 `spindown_time`；保留定期 TRIM，用 `systemctl status fstrim.timer` 检查。关闭 APM 不保证提升吞吐。

## SATA 链路：可选的性能优先设置

链路省电与盘内 APM 是两层设置。先查询内核当前值，并确认目标盘对应的控制器和 host：

```sh
grep . /sys/class/scsi_host/host*/link_power_management_policy
readlink -f /sys/class/block/sdX/device
readlink -f /sys/class/scsi_host/hostN
```

将 `sdX`、`hostN` 替换为刚确认的设备。对支持配置的目标 host 立即应用：

```sh
echo max_performance | sudo tee /sys/class/scsi_host/hostN/link_power_management_policy
```

若返回“不支持”，不要强行设置。需要整个 SATA 控制器使用性能优先时，在 `/etc/udev/rules.d/99-nas-disk-power.rules` 写入以下规则；将 `0000:00:1f.2` 换为上述真实路径中的 PCI 控制器地址。这会影响该控制器下的 HDD 和 SSD，而不只是一个盘。

```udev
ACTION=="add", SUBSYSTEM=="scsi_host", KERNELS=="0000:00:1f.2", TEST=="link_power_management_policy", ATTR{link_power_management_policy}!="keep_firmware_settings", ATTR{link_power_management_policy}="max_performance"
```

这里保留处于 `keep_firmware_settings` 的端口，避免对未提供可调策略的端口写入；不能据此宣称所有控制器都支持这项配置。

## USB 机械盘：同时处理硬盘和 USB 自动挂起

先对稳定的整盘 `by-id` 路径执行前述 `hdparm -B 255 -S 0`，再读回 APM。USB 桥接芯片可能不支持 ATA 命令，出现 `SG_IO` 等报错时，不能认为所有参数均已生效；即使 APM 读回成功，也不能证明停转计时器或硬盘盒自己的休眠策略已关闭。

Debian 13 的 hdparm 辅助脚本默认跳过 USB APM。仅对已确认支持的盘，在同一个 udev 规则文件中添加精确匹配规则。先获取属性：

```sh
udevadm info --query=property --name=/dev/disk/by-id/usb-YOUR_DISK
udevadm info --attribute-walk --name=/dev/disk/by-id/usb-YOUR_DISK
```

从第一条输出取 `ID_SERIAL`；从第二条输出中同一个 USB 设备父节点取 `idVendor`、`idProduct` 和 `serial`。USB 外壳序列号与硬盘 ATA 序列号可能不同，不能混用。替换下面所有大写占位值：

```udev
ACTION=="add", SUBSYSTEM=="block", ENV{DEVTYPE}=="disk", ENV{ID_SERIAL}=="EXACT_ID_SERIAL", RUN+="/usr/sbin/hdparm -B 255 -S 0 /dev/%k"
ACTION=="add", SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="VENDOR", ATTR{idProduct}=="PRODUCT", ATTR{serial}=="USB_SERIAL", TEST=="power/control", ATTR{power/control}="on"
```

如果只能使用 APM `254`，相应修改命令。外壳没有唯一序列号时，不要用通配符扩大到所有 USB 设备。按唯一标识匹配后，换 USB 插口不影响规则。

从属性查询结果找到该外壳对应的 `/sys/bus/usb/devices/` 节点，例如 `4-2`，立即关闭它的自动挂起：

```sh
echo on | sudo tee /sys/bus/usb/devices/4-2/power/control
cat /sys/bus/usb/devices/4-2/power/control
cat /sys/bus/usb/devices/4-2/power/runtime_status
```

`control=on` 禁止该设备自动挂起，`runtime_status=active` 表示当前活跃。此时即使 `autosuspend_delay_ms` 仍为 `2000`，也不会按该延时挂起。设备节点随插口变化，检查时重新定位；持久化规则不依赖这个节点名。

## 加载、验证与撤销

```sh
sudo udevadm verify /etc/udev/rules.d/99-nas-disk-power.rules
sudo udevadm control --reload-rules
```

重新加载规则不会给已有设备立即应用设置，需要执行对应的手动命令。可用 `sudo udevadm test --action=add /sys/class/block/sdX` 检查块设备规则匹配；测试模式不执行 `RUN` 命令，不能替代实际状态验证。

维护窗口重启后，再读取 APM、SATA 链路策略和 USB 电源状态，确认自动应用成功。测试 USB 重插前，停止使用该盘的服务（包括 Tier、Samba）并卸载，挂回并确认挂载正确后再恢复服务。挂载使用文件系统 UUID，不依赖 `/dev/sdX`。

停转计时器通常无法像 APM 一样可靠读回；`hdparm -C` 只能报告当前电源状态，不能证明永不休眠。仍需观察真实闲置后读写及内核日志，排除硬盘盒独立计时器或其他电源管理工具覆盖配置。不要用持续读写用户文件的方式“保活”。

撤销时恢复备份的 `hdparm.conf`，删除本次增加的 udev 规则并重新加载；已生效的运行时值不会因此立刻回退。按记录恢复原值，或在维护窗口重启后重新核对。

参数依据：[hdparm 手册](https://man7.org/linux/man-pages/man8/hdparm.8.html)、[Linux USB 电源管理文档](https://www.kernel.org/doc/html/latest/driver-api/usb/power-management.html)。
