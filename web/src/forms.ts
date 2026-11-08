export type Field = {
  key: string;
  label: string;
  kind: "text" | "list" | "number" | "checkbox";
  hint?: string;
  section?: string;
  cacheOnly?: boolean;
  placeholder?: string;
  optional?: boolean;
  defaultValue?: number;
  min?: number;
  max?: number;
};
export type Input = string | number | boolean;
export function initialValue(field: Field, current: unknown): Input {
  if (field.kind === "checkbox") return current === true;
  if (field.kind === "list")
    return Array.isArray(current) ? current.join(", ") : "";
  if (field.kind === "number")
    return typeof current === "number" ? current : (field.defaultValue ?? "");
  return typeof current === "string" ? current : "";
}
export function serialize(field: Field, input: Input) {
  switch (field.kind) {
    case "list":
      return String(input)
        .split(",")
        .map((v) => v.trim())
        .filter(Boolean);
    case "number":
      return input === "" ? null : Number(input);
    case "checkbox":
      return input === true;
    case "text":
      return String(input);
  }
}
export function hasCache(values: Record<string, Input>) {
  return String(values.write_cache ?? "")
    .split(",")
    .some((name) => name.trim() !== "");
}
export function policyInput(values: Record<string, Input>) {
  const input = Object.fromEntries(
    forms.policy.map((field) => [
      field.key,
      serialize(field, values[field.key]),
    ]),
  );
  if (!hasCache(values))
    Object.assign(input, {
      write_cache: [],
      cache_copies: 0,
      persist_delay_secs: 0,
      direct_fallback: false,
      fallback_count: null,
    });
  if (!values.direct_fallback) input.fallback_count = null;
  return input;
}
export const forms: Record<string, Field[]> = {
  repair: [],
  inspect: [
    {
      key: "path",
      label: "快照文件",
      placeholder: "/mnt/backup/tier.json",
      kind: "text",
    },
    {
      key: "roots",
      hint: "填写实际数据盘的挂载目录，例如 /mnt/disk1。",
      label: "本地设备目录（可留空，多个用逗号分隔）",
      optional: true,
      kind: "list",
    },
  ],
  archive: [
    { key: "name", label: "归档名称", placeholder: "cloud1", kind: "text" },
    {
      key: "url",
      hint: "填写已有的可读写 WebDAV 集合地址。",
      label: "WebDAV 地址",
      placeholder: "https://nas.example/dav/tier",
      kind: "text",
    },
    {
      key: "user_env",
      hint: "填写运行 Tier 服务时设置的环境变量名，例如 WEBDAV_USER。",
      label: "用户名环境变量（可选）",
      optional: true,
      kind: "text",
    },
    {
      key: "pass_env",
      hint: "填写密码所在的环境变量名，例如 WEBDAV_PASSWORD。",
      label: "密码环境变量（可选）",
      optional: true,
      kind: "text",
    },
  ],
  cloud_policy: [
    {
      key: "path",
      label: "共享内目录",
      hint: "填写共享里的路径，例如 /照片；子目录默认继承最近一级策略。",
      placeholder: "/",
      kind: "text",
    },
    {
      key: "targets",
      hint: "填写已登记的 WebDAV 归档名称。",
      label: "归档目标（逗号分隔）",
      optional: true,
      placeholder: "cloud1",
      kind: "list",
    },
    {
      defaultValue: 1,
      min: 0,
      key: "copies",
      hint: "归档目标中要保存的副本数。",
      label: "归档份数",
      kind: "number",
    },
    {
      defaultValue: 7776000,
      min: 0,
      key: "delay_secs",
      hint: "从文件最后一次修改起计算，300 秒为 5 分钟。",
      label: "归档延迟（秒）",
      kind: "number",
    },
    {
      defaultValue: 1,
      min: 0,
      key: "retain_local",
      hint: "设为 0 允许校验远端对象后释放最后一份本地数据。",
      label: "保留本地份数",
      kind: "number",
    },
  ],
  backup_schedule: [
    {
      key: "directories",
      hint: "填写已存在且可写的绝对路径，建议选择不同设备上的目录。",
      label: "备份目录（逗号分隔）",
      optional: true,
      placeholder: "/mnt/a/backup,/mnt/b/backup",
      kind: "list",
    },
    {
      defaultValue: 30,
      min: 1,
      key: "seconds",
      hint: "有元数据变更时，等待达到此秒数或累计提交数即触发快照。",
      label: "触发间隔（秒）",
      kind: "number",
    },
    {
      defaultValue: 100,
      min: 1,
      key: "changes",
      hint: "从上次快照起累计的元数据事务数，达到阈值即可提前导出。",
      label: "累计元数据提交数",
      kind: "number",
    },
    {
      defaultValue: 20,
      min: 1,
      key: "keep",
      hint: "每个目的地保留最近多少份快照。",
      label: "最近保留份数",
      kind: "number",
    },
    {
      defaultValue: 0,
      min: 0,
      key: "retain_days",
      hint: "保留最近几天的全部快照。0 表示关闭；最近份数和分时规则仍然生效。",
      label: "全部保留天数",
      kind: "number",
    },
    {
      defaultValue: 0,
      min: 0,
      key: "hourly",
      hint: "在此时间窗口内按小时保留快照，0 表示关闭。",
      label: "按小时保留的窗口（小时）",
      kind: "number",
    },
    {
      defaultValue: 0,
      min: 0,
      key: "daily",
      hint: "在此时间窗口内按天保留快照，0 表示关闭。",
      label: "按天保留的窗口（天）",
      kind: "number",
    },
  ],
  snapshot: [
    {
      key: "path",
      label: "快照目的文件",
      placeholder: "/mnt/backup/tier.json",
      kind: "text",
    },
  ],
  restore: [
    {
      defaultValue: 1,
      min: 1,
      key: "id",
      label: "删除条目编号",
      kind: "number",
    },
    {
      key: "path",
      label: "恢复位置",
      placeholder: "/照片/example.jpg",
      kind: "text",
    },
  ],
  purge: [
    {
      defaultValue: 1,
      min: 1,
      key: "id",
      label: "删除条目编号",
      kind: "number",
    },
  ],
  policy: [
    {
      key: "path",
      label: "共享内目录",
      hint: "填写共享里的路径，例如 /照片；子目录默认继承最近一级策略。",
      placeholder: "/",
      kind: "text",
    },
    {
      key: "persistent_storage",
      section: "持久化存储",
      label: "持久化目标",
      placeholder: "disk1,disk2",
      kind: "list",
      hint: "选择最终保存文件的设备。没有写入缓存时，写入直接在这些设备上确认。",
    },
    {
      key: "persistent_copies",
      label: "持久化副本数",
      kind: "number",
      min: 1,
      defaultValue: 1,
      hint: "双盘同步写入时，选择两块持久化设备并设为 2。",
    },
    {
      key: "write_cache",
      section: "写入缓存（可选）",
      label: "缓存目标",
      placeholder: "可留空",
      optional: true,
      kind: "list",
      hint: "留空时直接写入持久化存储。启用后，先写入所选缓存，再后台复制到持久化存储。",
    },
    {
      key: "cache_copies",
      label: "缓存确认份数",
      kind: "number",
      min: 1,
      defaultValue: 1,
      cacheOnly: true,
      hint: "缓存模式下，此份数写入完成后向客户端确认。",
    },
    {
      key: "persist_delay_secs",
      label: "持久化延迟（秒）",
      kind: "number",
      min: 0,
      defaultValue: 1800,
      cacheOnly: true,
      hint: "文件停止修改后再等待指定秒数，开始创建持久化副本。",
    },
    {
      key: "low_water",
      section: "缓存淘汰策略",
      label: "后台淘汰阈值（已用 %）",
      kind: "number",
      min: 0,
      max: 100,
      defaultValue: 70,
      cacheOnly: true,
      hint: "缓存设备达到此使用率后，后台逐步清理已有足够副本的数据。",
    },
    {
      key: "high_water",
      label: "同步淘汰阈值（已用 %）",
      kind: "number",
      min: 0,
      max: 100,
      defaultValue: 90,
      cacheOnly: true,
      hint: "缓存设备达到此使用率时，写入前尝试释放空间。应高于后台淘汰阈值。",
    },
    {
      key: "direct_fallback",
      label: "缓存不足时直写持久化存储",
      kind: "checkbox",
      cacheOnly: true,
      hint: "关闭时，缓存空间不足会导致此次写入失败。",
    },
    {
      key: "fallback_count",
      label: "直写确认份数",
      kind: "number",
      min: 1,
      optional: true,
      cacheOnly: true,
      hint: "留空时使用持久化副本数。",
    },
    {
      section: "删除与修复",
      defaultValue: 604800,
      min: 0,
      key: "trash_ttl_secs",
      hint: "删除后保留多久，例如 604800 秒为 7 天。",
      label: "回收站保留时间（秒）",
      kind: "number",
    },
    {
      key: "manual_repair",
      hint: "启用后，由管理员在控制台触发故障修复。",
      label: "手动修复",
      kind: "checkbox",
    },
    {
      defaultValue: 0,
      min: 0,
      key: "repair_grace_secs",
      hint: "设备暂时离线时，等待这段时间再启动自动修复。",
      label: "故障修复宽限期（秒）",
      kind: "number",
    },
  ],
  inherit: [
    {
      key: "path",
      label: "共享内目录",
      hint: "填写共享里的路径，例如 /照片；子目录默认继承最近一级策略。",
      placeholder: "/照片",
      kind: "text",
    },
    {
      key: "archive",
      label: "恢复归档策略继承（不选则恢复本地策略继承）",
      kind: "checkbox",
    },
  ],
  target: [
    { key: "name", label: "目标名称", placeholder: "disk1", kind: "text" },
    {
      key: "path",
      label: "已存在的 Linux 目录",
      placeholder: "/mnt/disk1",
      kind: "text",
    },
    {
      key: "domain",
      hint: "共用同一故障风险的设备可填相同名称；留空由 Tier 检测。",
      label: "故障域（留空自动识别）",
      optional: true,
      kind: "text",
    },
    {
      key: "allow_same_domain",
      hint: "同一设备上的两个目录可能随一次故障同时离线。",
      label: "接受多个目标位于同一设备",
      kind: "checkbox",
    },
  ],
};
