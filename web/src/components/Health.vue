<script setup lang="ts">
import { ref, computed } from "vue";
import { statuses, needsAttention, type Health } from "../model";
const props = defineProps<{ items: Health[] }>();
defineEmits<{ repair: [] }>();
const filter = ref("attention");
const filtered = computed(() =>
  filter.value === "all"
    ? props.items
    : props.items.filter((x) => needsAttention(x.status)),
);
</script>
<template>
  <div class="section-head">
    <h2>文件健康</h2>
    <div class="actions">
      <button class="primary" @click="$emit('repair')">修复副本</button>
      <button
        @click="filter = 'attention'"
        :aria-pressed="filter === 'attention'"
      >
        需处理
      </button>
      <button @click="filter = 'all'" :aria-pressed="filter === 'all'">
        全部文件
      </button>
    </div>
  </div>
  <div class="scroll" v-if="filtered.length">
    <table>
      <thead>
        <tr>
          <th>文件路径</th>
          <th>状态</th>
          <th>持久化副本</th>
          <th>可读设备</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="f in filtered" :key="f.path">
          <td>{{ f.path }}</td>
          <td>
            <span class="badge" :class="{ warn: needsAttention(f.status) }">
              {{ statuses[f.status] ?? f.status }}
            </span>
            <small v-if="f.error" class="detail">{{ f.error }}</small>
          </td>
          <td>{{ f.current }} / {{ f.required }}</td>
          <td>
            {{ f.readable_local.join("、") || "—" }}
            <small v-if="f.readable_local.length" class="detail">
              {{ f.readable_domains }} 个故障域
            </small>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
  <p v-else class="empty">
    {{ filter === "all" ? "共享目录还没有文件。" : "当前没有需要处理的文件。" }}
  </p>
</template>
