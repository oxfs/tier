<script setup lang="ts">
import { type State } from "../model";
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
    <h2>WebDAV 目标</h2>
    <div class="grid">
      <article class="entity" v-for="a in state.cloud_targets" :key="a.name">
        <h3>{{ a.name }}</h3>
        <p>{{ a.url }}</p>
        <small>
          已登记
          {{ a.object_count }}
          个对象
        </small>
      </article>
    </div>
    <p v-if="!state.cloud_targets.length" class="empty">尚未配置归档目标。</p>
  </section>
  <section class="panel">
    <h2>归档策略</h2>
    <div v-for="p in state.cloud_policy" :key="p.node" class="entity">
      <h3>{{ p.path }}</h3>
      <p>
        归档 {{ p.settings.copies }} 份，保留本地
        {{ p.settings.retain_local }} 份
      </p>
      <p class="muted">
        {{ (p.settings.targets as string[]).join("、") }} · 延迟
        {{ p.settings.delay_secs }} 秒
      </p>
      <button
        @click="
          open('cloud_policy', {
            ...p.settings,
            path: p.path,
          })
        "
      >
        编辑归档策略
      </button>
    </div>
    <p v-if="!state.cloud_policy.length" class="empty">暂无归档策略。</p>
  </section>

  <section class="panel">
    <h2>归档异常</h2>
    <p v-for="e in state.archive_errors" :key="e.node">
      {{ e.path }} · {{ e.error }}
    </p>
    <p v-if="!state.archive_errors.length" class="empty">
      没有已报告的归档错误。
    </p>
  </section>
</template>
