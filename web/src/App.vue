<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import {
  Activity,
  Archive,
  ArrowUpRight,
  HardDrive,
  Layers,
  LayoutDashboard,
  RefreshCw,
  Trash2,
} from "@lucide/vue";
import ActionDialog from "./components/ActionDialog.vue";
import Devices from "./components/Devices.vue";
import TrashView from "./views/TrashView.vue";
import ArchivesView from "./views/ArchivesView.vue";
import BackupsView from "./views/BackupsView.vue";
import Health from "./components/Health.vue";
import {
  pages,
  actions,
  bytes,
  needsAttention,
  type State,
  type Page,
} from "./model";
function current(): Page {
  const key = location.hash.slice(1);
  return Object.hasOwn(pages, key) ? (key as Page) : "overview";
}
const page = ref<Page>(current()),
  state = ref<State>(),
  message = ref(""),
  loading = ref(false),
  action = ref(""),
  initial = ref<Record<string, unknown>>({});
function navigate() {
  page.value = current();
  document.title = pages[page.value] + " · Tier";
}
let queuedRefresh = false;
async function refresh() {
  if (loading.value) {
    queuedRefresh = true;
    return;
  }
  loading.value = true;
  try {
    const response = await fetch("/api/state", { cache: "no-store" });
    if (!response.ok)
      throw Error((await response.json()).error ?? response.statusText);
    state.value = await response.json();
    message.value = "";
  } catch (e) {
    message.value = "状态读取失败 · " + String(e);
  } finally {
    loading.value = false;
    if (queuedRefresh) {
      queuedRefresh = false;
      void refresh();
    }
  }
}
onMounted(() => {
  addEventListener("hashchange", navigate);
  navigate();
  refresh();
});
onUnmounted(() => removeEventListener("hashchange", navigate));
function open(key: string, data: Record<string, unknown> = {}) {
  initial.value = data;
  action.value = key;
}
const capacity = computed(() => {
  const devices = new Map(
    (state.value?.targets ?? [])
      .filter(
        (t) =>
          t.online &&
          t.device != null &&
          t.total_bytes != null &&
          t.available_bytes != null,
      )
      .map((t) => [t.device, t]),
  );
  if (!devices.size) return null;
  return [...devices.values()].reduce(
    (a, t) => ({
      total: a.total + t.total_bytes!,
      available: a.available + t.available_bytes!,
    }),
    { total: 0, available: 0 },
  );
});
const problems = computed(() => {
  const data = state.value;
  if (!data) return 0;
  const paths = new Set(
    data.health.filter((f) => needsAttention(f.status)).map((f) => f.path),
  );
  for (const failure of data.archive_errors) paths.add(failure.path);
  return paths.size;
});
function focusContent() {
  document.getElementById("content")?.focus();
}
function refreshOnReturn() {
  if (!document.hidden) refresh();
}
onMounted(() => document.addEventListener("visibilitychange", refreshOnReturn));
onUnmounted(() =>
  document.removeEventListener("visibilitychange", refreshOnReturn),
);
const shortcuts: Record<Page, string[]> = {
  overview: [],
  devices: ["target"],
  policies: ["policy", "inherit"],
  archives: ["archive", "archive_policy", "inherit"],
  backups: ["snapshot", "backup_schedule", "inspect"],
  trash: [],
};
const icons = {
  overview: LayoutDashboard,
  devices: HardDrive,
  policies: Layers,
  archives: Archive,
  backups: Activity,
  trash: Trash2,
};
</script>
<template>
  <a class="skip" href="#content" @click.prevent="focusContent">跳到主内容</a>
  <aside>
    <div class="brand">
      <span class="mark" aria-hidden="true">
        <i></i>
        <i></i>
        <i></i>
      </span>
      Tier
    </div>
    <nav aria-label="主导航">
      <a
        v-for="(label, key) in pages"
        :key="key"
        :href="'#' + key"
        :aria-current="page === key ? 'page' : undefined"
      >
        <component
          :is="icons[key]"
          :size="17"
          :stroke-width="1.7"
          aria-hidden="true"
        />
        {{ label }}
      </a>
    </nav>
  </aside>
  <main id="content" tabindex="-1">
    <header>
      <div>
        <div class="crumb">
          Tier
          <span>/</span>
          控制台
        </div>
        <h1>{{ pages[page] }}</h1>
      </div>
      <button class="refresh" @click="refresh" :disabled="loading">
        <RefreshCw :size="15" aria-hidden="true" />
        {{ loading ? "正在刷新…" : "刷新状态" }}
      </button>
    </header>
    <p v-if="message" role="alert" class="notice">{{ message }}</p>
    <template v-if="state">
      <div class="metrics" v-if="page === 'overview'">
        <div class="metric">
          <small>在线设备可用容量</small>
          <strong>{{ bytes(capacity?.available) }}</strong>
          <small>总容量 {{ bytes(capacity?.total) }}</small>
        </div>
        <div class="metric">
          <small>在线 / 已登记设备</small>
          <strong>
            {{ state.targets.filter((t) => t.online).length }} /
            {{ state.targets.length }}
          </strong>
        </div>
        <div class="metric">
          <small>待处理文件</small>
          <strong>{{ problems }}</strong>
          <a v-if="state.archive_errors.length" href="#archives">
            归档异常 {{ state.archive_errors.length }} 项 ↗
          </a>
          <small v-else>副本不足或写入异常</small>
        </div>
      </div>
      <div v-if="shortcuts[page].length" class="toolbar actions">
        <button
          v-for="(key, i) in shortcuts[page]"
          :key="key"
          :class="{ primary: i === 0 }"
          @click="
            open(key, key === 'inherit' ? { archive: page === 'archives' } : {})
          "
        >
          {{ actions[key] }}
        </button>
      </div>
      <template v-if="page === 'overview' || page === 'devices'">
        <section class="panel">
          <div class="section-head">
            <h2>存储设备</h2>
            <a v-if="page === 'overview'" href="#devices">
              管理设备
              <ArrowUpRight :size="14" aria-hidden="true" />
            </a>
          </div>
          <Devices :targets="state.targets" />
        </section>
        <section v-if="page === 'overview'" class="panel">
          <Health :items="state.health" @repair="open('repair')" />
        </section>
      </template>
      <section v-if="page === 'policies'" class="panel">
        <div class="section-head"><h2>目录绑定</h2></div>
        <div class="grid">
          <article class="entity" v-for="p in state.policies" :key="p.node">
            <h3>{{ p.path }}</h3>
            <p v-if="p.policy.persistent_storage.length">
              持久化存储 {{ p.policy.persistent_copies }} 份 ·
              {{ p.policy.persistent_storage.join("、") }}
            </p>
            <p v-else>尚未配置持久化存储</p>
            <p v-if="p.policy.write_cache.length">
              缓存 ·
              {{ p.policy.write_cache.join("、") }}
            </p>
            <p v-if="p.policy.persistent_storage.length" class="muted">
              关闭后 {{ p.policy.persist_delay_secs }} 秒补齐持久化副本
            </p>
            <button @click="open('policy', { ...p.policy, path: p.path })">
              编辑策略
            </button>
          </article>
        </div>
        <p v-if="!state.policies.length" class="empty">
          尚未绑定副本策略，请先配置共享根目录。
        </p>
      </section>
      <ArchivesView v-if="page === 'archives'" :state="state" @action="open" />
      <BackupsView v-if="page === 'backups'" :state="state" @action="open" />
      <TrashView v-if="page === 'trash'" :state="state" @action="open" />
    </template>
    <p v-else-if="loading" role="status">正在读取存储状态…</p>
  </main>
  <ActionDialog
    v-if="action"
    :action="action"
    :initial="initial"
    :state="state"
    @close="action = ''"
    @updated="refresh"
  />
</template>
