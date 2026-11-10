export interface Target {
  name: string;
  path: string;
  domain: string;
  online: boolean;
  device: number | null;
  total_bytes: number | null;
  available_bytes: number | null;
}
export interface Health {
  path: string;
  status: string;
  current: number;
  required: number;
  readable_local: string[];
  readable_domains: number;
  repair: string;
  error: string | null;
}
export interface Policy {
  node: string;
  path: string;
  policy: {
    write_cache: string[];
    persistent_storage: string[];
    persistent_copies: number;
    persist_delay_secs: number;
  };
}
export interface ArchivePolicy {
  node: string;
  path: string;
  settings: {
    copies: number;
    targets: string[];
    retain_local: number;
    delay_secs: number;
  };
}
export interface Archive {
  name: string;
  url: string;
  object_count: number;
}
export interface Trash {
  id: number;
  original: string;
  expires: number;
  logical_bytes: number;
  target_bytes: Record<string, number | null>;
}
export interface Backup {
  kind: BackupKind;
  path: string;
  time: number;
  error: string | null;
  attempted_at: number;
  latest: string | null;
}
export type BackupKind = "export" | "metadata";
export const backupKinds = { export: "文件映射导出", metadata: "元数据备份" };
export interface State {
  targets: Target[];
  health: Health[];

  policies: Policy[];
  archive_policy: ArchivePolicy[];
  archive_targets: Archive[];

  archive_errors: { node: string; path: string; error: string }[];
  backup_schedules: {
    kind: BackupKind;
    config: {
      enabled: boolean;
      seconds: number;
      keep: number;
      directories: string[];
    };
  }[];
  backup_status: Backup[];
  trash_count: number;
}
export const pages = {
  overview: "存储概览",
  devices: "存储设备",
  policies: "副本策略",
  archives: "归档",
  backups: "元数据",
  trash: "回收站",
};
export type Page = keyof typeof pages;
export const actions: Record<string, string> = {
  repair: "修复副本",
  inspect: "恢复预检",
  archive: "添加归档目标",
  archive_policy: "配置归档策略",
  backup_schedule: "配置计划",
  backup: "立即执行计划",
  export: "导出文件映射",
  restore: "恢复文件",
  purge: "永久删除",
  policy: "配置副本策略",
  inherit: "恢复继承",
  target: "添加存储设备",
};
export const statuses: Record<string, string> = {
  healthy: "已保护",
  waiting: "待保护",
  writing: "正在写入",
  remote_only: "仅远端",
  needs_attention: "待修复",
  under_replicated: "副本不足",
};
export const needsAttention = (s: string) =>
  ["needs_attention", "under_replicated"].includes(s);
export function bytes(n: number | null | undefined) {
  if (n == null) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return (
    n.toLocaleString("zh-CN", { maximumFractionDigits: 1 }) + " " + units[i]
  );
}
export const date = (n: number) =>
  n
    ? new Date(n * 1000).toLocaleString("zh-CN", { hour12: false })
    : "尚未完成";
