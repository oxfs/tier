<script setup lang="ts">
import { bytes, type Target } from "../model";
defineProps<{ targets: Target[] }>();
</script>
<template>
  <div class="grid" v-if="targets.length">
    <article class="entity" v-for="t in targets" :key="t.name">
      <div class="entity-head">
        <h3>{{ t.name }}</h3>
        <span class="badge" :class="{ bad: !t.online }">
          {{ t.online ? "在线" : "离线" }}
        </span>
      </div>
      <p class="mono">{{ t.path }}</p>
      <template v-if="t.online && t.total_bytes && t.available_bytes != null">
        <div class="bar">
          <i
            :style="{
              width: 100 - (t.available_bytes / t.total_bytes) * 100 + '%',
            }"
          ></i>
        </div>
        <small>
          可用 {{ bytes(t.available_bytes) }} / {{ bytes(t.total_bytes) }}
        </small>
      </template>
      <p class="muted">故障域 · {{ t.domain }}</p>
    </article>
  </div>
  <p class="empty" v-else>尚未登记存储设备。</p>
</template>
