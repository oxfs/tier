<script setup lang="ts">
import { watch, onUnmounted, ref } from "vue";
import { bytes, date, type Trash, type State } from "../model";
const props = defineProps<{ state: State }>();
const emit = defineEmits<{
  action: [action: string, initial: Record<string, unknown>];
}>();
const result = ref<{
    items: Trash[];
    page: number;
    pages: number;
    total: number;
  }>(),
  error = ref(""),
  loading = ref(false);
let active: AbortController | undefined;
let current = 1;
async function load(page: number) {
  current = page;
  active?.abort();
  const request = new AbortController();
  active = request;
  loading.value = true;
  error.value = "";
  try {
    const response = await fetch(`/api/trash?page=${page}`, {
      signal: request.signal,
      cache: "no-store",
    });
    if (!response.ok) throw Error("回收站读取失败");
    const data = await response.json();
    if (!request.signal.aborted) {
      result.value = data;
      current = data.page;
    }
  } catch (e) {
    if (!request.signal.aborted) error.value = String(e);
  } finally {
    if (!request.signal.aborted) loading.value = false;
  }
}
watch(
  () => props.state,
  () => load(current),
  { immediate: true },
);
onUnmounted(() => active?.abort());
</script>
<template>
  <section class="panel">
    <h2>删除条目</h2>
    <div class="scroll" v-if="result?.items.length">
      <table>
        <thead>
          <tr>
            <th>原路径</th>
            <th>逻辑大小</th>
            <th>各设备占用</th>
            <th>到期时间</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="t in result.items" :key="t.id">
            <td>{{ t.original }}</td>
            <td>{{ bytes(t.logical_bytes) }}</td>
            <td>
              <div v-for="(n, k) in t.target_bytes" :key="k">
                {{ k }} · {{ bytes(n) }}
              </div>
            </td>
            <td>{{ date(t.expires) }}</td>
            <td>
              <div class="actions">
                <button
                  @click="
                    emit('action', 'restore', { id: t.id, path: t.original })
                  "
                >
                  恢复
                </button>
                <button @click="emit('action', 'purge', { id: t.id })">
                  清理
                </button>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <p v-if="error" role="alert">{{ error }}</p>
    <p v-else-if="loading" role="status">正在读取回收站…</p>
    <p v-else-if="!result?.total" class="empty">回收站为空。</p>
    <nav v-if="result?.total" aria-label="回收站分页" class="trash-pagination">
      <button
        :disabled="loading || result.page <= 1"
        @click="load(result.page - 1)"
      >
        上一页
      </button>
      <span aria-live="polite">
        第 {{ result.page }} / {{ result.pages }} 页 · 共 {{ result.total }} 项
      </span>
      <button
        :disabled="loading || result.page >= result.pages"
        @click="load(result.page + 1)"
      >
        下一页
      </button>
    </nav>
  </section>
</template>
<style scoped>
.trash-pagination {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 1rem;
  margin-top: 1rem;
}
</style>
