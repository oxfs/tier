export type Field = {
  key: string;
  label: string;
  kind: "text" | "list" | "number" | "checkbox";
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
      label: "本地设备目录（可留空，多个用逗号分隔）",
      optional: true,
      kind: "list",
    },
  ],
  archive: [
    { key: "name", label: "归档名称", placeholder: "cloud1", kind: "text" },
    {
      key: "url",
      label: "WebDAV 地址",
      placeholder: "https://nas.example/dav/tier",
      kind: "text",
    },
    {
      key: "user_env",
      label: "用户名环境变量（可选）",
      optional: true,
      kind: "text",
    },
    {
      key: "pass_env",
      label: "密码环境变量（可选）",
      optional: true,
      kind: "text",
    },
  ],
  cloud_policy: [
    { key: "path", label: "目录", placeholder: "/", kind: "text" },
    {
      key: "targets",
      label: "归档目标（逗号分隔）",
      optional: true,
      placeholder: "cloud1",
      kind: "list",
    },
    {
      defaultValue: 1,
      min: 0,
      key: "copies",
      label: "归档份数",
      kind: "number",
    },
    {
      defaultValue: 7776000,
      min: 0,
      key: "delay_secs",
      label: "归档延迟（秒）",
      kind: "number",
    },
    {
      defaultValue: 1,
      min: 0,
      key: "retain_local",
      label: "保留本地份数",
      kind: "number",
    },
  ],
  backup_schedule: [
    {
      key: "directories",
      label: "备份目录（逗号分隔）",
      optional: true,
      placeholder: "/mnt/a/backup,/mnt/b/backup",
      kind: "list",
    },
    {
      defaultValue: 30,
      min: 1,
      key: "seconds",
      label: "触发间隔（秒）",
      kind: "number",
    },
    {
      defaultValue: 100,
      min: 1,
      key: "changes",
      label: "累计元数据提交数",
      kind: "number",
    },
    {
      defaultValue: 20,
      min: 1,
      key: "keep",
      label: "最近保留份数",
      kind: "number",
    },
    {
      defaultValue: 0,
      min: 0,
      key: "hourly",
      label: "按小时保留的窗口（小时）",
      kind: "number",
    },
    {
      defaultValue: 0,
      min: 0,
      key: "daily",
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
    { key: "path", label: "目录", placeholder: "/", kind: "text" },
    {
      key: "front",
      label: "前台目标（逗号分隔）",
      optional: false,
      placeholder: "ssd",
      kind: "list",
    },
    {
      defaultValue: 1,
      min: 1,
      key: "front_count",
      label: "前台确认份数",
      kind: "number",
    },
    {
      key: "long",
      label: "长期目标（逗号分隔）",
      optional: true,
      placeholder: "disk1,disk2",
      kind: "list",
    },
    {
      defaultValue: 2,
      min: 0,
      key: "long_count",
      label: "长期份数",
      kind: "number",
    },
    {
      defaultValue: 1800,
      min: 0,
      key: "delay_secs",
      label: "安静期（秒）",
      kind: "number",
    },
    {
      label: "慢速回收已用比例（%）",
      kind: "number",
      max: 100,
      key: "low_water",
      defaultValue: 70,
      min: 0,
    },
    {
      label: "同步回收已用比例（%）",
      kind: "number",
      max: 100,
      key: "high_water",
      defaultValue: 90,
      min: 0,
    },
    {
      defaultValue: 604800,
      min: 0,
      key: "trash_ttl_secs",
      label: "回收保留时间（秒）",
      kind: "number",
    },
    { key: "manual_repair", label: "手动修复", kind: "checkbox" },
    {
      defaultValue: 0,
      min: 0,
      key: "repair_grace_secs",
      label: "故障修复宽限期（秒）",
      kind: "number",
    },
    {
      key: "fallback_count",
      min: 1,
      label: "直写确认份数（默认与长期份数相同）",
      optional: true,
      kind: "number",
    },
    {
      key: "direct_fallback",
      label: "暂存不足时直接写长期池",
      kind: "checkbox",
    },
  ],
  inherit: [
    { key: "path", label: "目录", placeholder: "/照片", kind: "text" },
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
      label: "故障域（留空自动识别）",
      optional: true,
      kind: "text",
    },
    {
      key: "allow_same_domain",
      label: "接受多个目标位于同一设备",
      kind: "checkbox",
    },
  ],
};
