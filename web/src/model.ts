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
  last_copy_cancelled: number | null;
  repair: string;
}
export interface Policy {
  node: string;
  path: string;
  policy: {
    front: string[];
    front_count: number;
    long: string[];
    long_count: number;
    delay_secs: number;
  };
}
export interface CloudPolicy {
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
  path: string;
  seq: number;
  time: number;
  error: string | null;
  lag: number;
}
export interface State {
  targets: Target[];
  health: Health[];

  policies: Policy[];
  cloud_policy: CloudPolicy[];
  cloud_targets: Archive[];

  archive_errors: { node: string; path: string; error: string }[];
  backup_schedule: {
    config: {
      seconds: number;
      changes: number;
      keep: number;
      hourly?: number;
      daily?: number;
      directories: string[];
    };
  }[];
  backup_status: Backup[];
  trash: Trash[];
  seq: number;
}
export const pages = {
  overview: "存储概览",
  devices: "存储设备",
  policies: "目录策略",
  archives: "云端归档",
  backups: "快照备份",
  trash: "回收站",
};
export type Page = keyof typeof pages;
export const actions: Record<string, string> = {
  repair: "修复副本",
  inspect: "恢复预检",
  archive: "添加归档目标",
  cloud_policy: "配置归档策略",
  backup_schedule: "配置备份计划",
  snapshot: "导出快照",
  restore: "恢复文件",
  purge: "永久删除",
  policy: "配置目录策略",
  inherit: "恢复继承",
  target: "添加存储设备",
};
export const statuses: Record<string, string> = {
  healthy: "健康",
  waiting: "等待复制",
  writing: "正在写入",
  unconfirmed: "写入未确认",
  remote_only: "仅远端",
  needs_attention: "待修复",
  under_replicated: "副本不足",
};
export const needsAttention = (s: string) =>
  ["unconfirmed", "needs_attention", "under_replicated"].includes(s);
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
