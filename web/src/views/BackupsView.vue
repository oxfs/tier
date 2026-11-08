<script setup lang="ts">
import { date, type State } from "../model";
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
    <h2>快照计划</h2>
    <article
      v-for="b in state.backup_schedule"
      :key="b.config.directories.join()"
      class="entity"
    >
      <p>{{ b.config.seconds }} 秒或 {{ b.config.changes }} 次提交触发</p>
      <p class="muted">
        最近 {{ b.config.keep }} 份 · 小时窗口 {{ b.config.hourly ?? 0 }} ·
        天窗口
        {{ b.config.daily ?? 0 }}
      </p>
      <p>{{ (b.config.directories as string[]).join("、") }}</p>
      <button @click="open('backup_schedule', b.config)">编辑计划</button>
    </article>
    <p v-if="!state.backup_schedule.length" class="empty">尚未配置自动快照。</p>
  </section>
  <section class="panel">
    <h2>备份目的地</h2>
    <div class="scroll" v-if="state.backup_status.length">
      <table>
        <thead>
          <tr>
            <th>目录</th>
            <th>最近成功</th>
            <th>序号</th>
            <th>落后提交</th>
            <th>状态</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="b in state.backup_status" :key="b.path">
            <td>{{ b.path }}</td>
            <td>{{ date(b.time) }}</td>
            <td>{{ b.seq }}</td>
            <td>{{ b.lag }}</td>
            <td>{{ b.error || "已发布" }}</td>
          </tr>
        </tbody>
      </table>
    </div>
    <p v-else class="empty">尚无快照发布记录。</p>
  </section>
</template>
