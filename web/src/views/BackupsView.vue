<script setup lang="ts">
import { backupKinds, date, type State } from "../model";
defineProps<{ state: State }>();
const emit = defineEmits<{
  action: [key: string, data: Record<string, unknown>];
}>();
function open(key: string, data: Record<string, unknown>) {
  emit("action", key, data);
}
</script>
<template>
  <section class="panel">
    <h2>自动保存计划</h2>
    <p class="muted">
      文件映射导出用于查找和迁移正文；元数据备份用于恢复
      Tier。两者均不包含正文。
    </p>
    <article v-for="b in state.backup_schedules" :key="b.kind" class="entity">
      <h3>{{ backupKinds[b.kind] }}</h3>
      <p>
        {{ b.config.enabled ? "已启用" : "已停用" }} · 每
        {{ b.config.seconds }} 秒
      </p>
      <p class="muted">最近 {{ b.config.keep }} 份</p>
      <p>{{ (b.config.directories as string[]).join("、") }}</p>
      <button @click="open('backup_schedule', { kind: b.kind, ...b.config })">
        编辑计划
      </button>
      <button
        :disabled="!b.config.directories.length"
        @click="open('backup', { kind: b.kind })"
      >
        立即执行
      </button>
    </article>
    <p v-if="!state.backup_schedules.length" class="empty">
      尚未配置自动保存计划。
    </p>
  </section>
  <section class="panel">
    <h2>保存记录</h2>
    <div class="scroll" v-if="state.backup_status.length">
      <table>
        <thead>
          <tr>
            <th>类型</th>
            <th>目录</th>
            <th>最近成功</th>
            <th>最近结果</th>
            <th>状态</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="b in state.backup_status" :key="b.kind + b.path">
            <td>{{ backupKinds[b.kind] }}</td>
            <td>{{ b.path }}</td>
            <td>{{ date(b.time) }}</td>
            <td>{{ b.latest || "—" }}</td>
            <td>{{ b.error || (b.time ? "已完成" : "尚未执行") }}</td>
          </tr>
        </tbody>
      </table>
    </div>
    <p v-else class="empty">尚无保存记录。</p>
  </section>
</template>
