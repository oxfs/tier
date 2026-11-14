<script setup>
import { ref, computed, onMounted, onUnmounted } from "vue";
import { NConfigProvider, NButton, NInput, NTag, NAlert } from "naive-ui";

const tab = ref("overview"),
  status = ref(null),
  content = ref(""),
  original = ref(""),
  revision = ref(""),
  error = ref(""),
  notice = ref(""),
  busy = ref(false);
const dirty = computed(() => content.value !== original.value);
const connectionError = ref("");
let refreshing = false;
const theme = {
  common: {
    primaryColor: "#267560",
    primaryColorHover: "#348b73",
    borderRadius: "8px",
    fontFamily: "Inter, Segoe UI, Microsoft YaHei, sans-serif",
  },
};
async function api(path, options = {}) {
  const response = await fetch("/api/" + path, {
    ...options,
    headers: {
      "Content-Type": "application/json",
      "X-Tier-Request": "1",
      ...options.headers,
    },
  });
  const value = await response.json();
  if (!response.ok) throw new Error(value.error || "请求失败");
  return value;
}
async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try {
    status.value = await api("health", { signal: AbortSignal.timeout(10000) });
    connectionError.value = "";
  } catch (e) {
    connectionError.value = e.message;
  } finally {
    refreshing = false;
  }
}
async function load() {
  try {
    const data = await api("config");
    content.value = original.value = data.content;
    revision.value = data.revision;
  } catch (e) {
    error.value = e.message;
  }
}
async function save(validate = false) {
  if (busy.value) return;
  const submitted = content.value;
  busy.value = true;
  error.value = "";
  notice.value = "";
  try {
    const data = await api(validate ? "validate" : "config", {
      method: "POST",
      body: JSON.stringify({ content: submitted, revision: revision.value }),
    });
    if (!validate) {
      original.value = submitted;
      revision.value = data.revision;
    }
    notice.value = validate
      ? "配置校验通过"
      : "已保存。后台将在下一轮扫描读取配置；Samba 配置变更需重启 smbd。";
    await refresh();
  } catch (e) {
    error.value = e.message;
  } finally {
    busy.value = false;
  }
}
async function maintenance(action, name) {
  if (busy.value) return;
  if (
    !window.confirm(
      action === "scan"
        ? "立即执行扫描和校验？"
        : "请确认已停止 SMB 访问。初始化仅用于新存储；恢复目标必须是已挂载的空目录。继续？",
    )
  )
    return;
  busy.value = true;
  error.value = "";
  notice.value = "";
  try {
    const result = await api(action, {
      method: "POST",
      body: JSON.stringify({ confirmed: true, name }),
    });
    notice.value = result.errors?.length
      ? result.errors.join("; ")
      : "操作完成";
    await refresh();
  } catch (e) {
    error.value = e.message;
  } finally {
    busy.value = false;
  }
}
const date = (value) =>
  value ? new Date(value * 1000).toLocaleString("zh-CN") : "尚未完成";
const bytes = (value) =>
  value == null ? "—" : (value / 1024 ** 3).toFixed(1) + " GB";
const leave = (e) => {
  if (dirty.value) {
    e.preventDefault();
    e.returnValue = "";
  }
};
let timer;
onMounted(() => {
  refresh();
  load();
  timer = setInterval(refresh, 5000);
  window.addEventListener("beforeunload", leave);
});
onUnmounted(() => {
  clearInterval(timer);
  window.removeEventListener("beforeunload", leave);
});
</script>

<template>
  <n-config-provider :theme-overrides="theme"
    ><div class="shell">
      <aside>
        <div class="brand">
          <span class="logo">▤</span> tier<span class="brand-dot">.</span>
        </div>
        <div class="subtitle">为珍贵记忆，留一份安心。</div>
        <nav>
          <button
            :class="{ active: tab === 'overview' }"
            @click="tab = 'overview'"
          >
            ◫ &nbsp; 存储概览</button
          ><button
            :class="{ active: tab === 'config' }"
            @click="tab = 'config'"
          >
            ⚙ &nbsp; 配置管理 <span v-if="dirty">•</span>
          </button>
        </nav>
        <div class="aside-bottom">
          PERSONAL STORAGE<br /><span>本地管理 · 无数据库</span>
        </div>
      </aside>
      <main>
        <header>
          <span>工作空间 / {{ tab === "overview" ? "概览" : "配置" }}</span
          ><n-button secondary size="small" @click="refresh">刷新状态</n-button>
        </header>
        <n-alert v-if="connectionError" type="warning" class="message"
          >连接中断，显示的是上次状态：{{ connectionError }}</n-alert
        >
        <n-alert v-if="error || status?.error" type="error" class="message">{{
          error || status.error
        }}</n-alert><n-alert
          v-if="notice"
          type="success"
          class="message"
          closable
          @close="notice = ''"
          >{{ notice }}</n-alert
        >
        <template v-if="tab === 'overview'"
          ><div class="heading">
            <div>
              <div class="eyebrow">YOUR MEMORIES, PROTECTED</div>
              <h1>每一份珍贵，都有归处。</h1>
              <p>查看存储设备与后台维护的实际运行状态。</p>
            </div>
            <n-tag :type="status?.healthy ? 'success' : 'warning'" round>{{
              status?.healthy ? "最近检查正常" : "需要关注 / 等待检查"
            }}</n-tag>
          </div>
          <div class="stats">
            <section>
              <label>本地存储</label
              ><strong
                >{{ status?.disks?.length ?? "—" }}
                <small>个目录</small></strong
              >
              <p>主盘与副本存储位置</p>
            </section>
            <section>
              <label>归档目标</label
              ><strong
                >{{ status?.archives?.length ?? "—" }}
                <small>个 WebDAV</small></strong
              >
              <p>连通性以归档任务结果为准</p>
            </section>
            <section>
              <label>目录策略</label
              ><strong
                >{{ status?.policies ?? "—" }} <small>条规则</small></strong
              >
              <p>按最长目录路径匹配</p>
            </section>
          </div>
          <section class="panel">
            <div class="panel-title">
              <h2>本地存储</h2>
              <span>容量与磁盘身份</span>
            </div>
            <div v-if="!status">正在读取状态…</div>
            <div v-for="disk in status?.disks" :key="disk.path" class="disk">
              <div class="disk-icon">▤</div>
              <div class="disk-info">
                <b>{{ disk.name }}</b
                ><code>{{ disk.path }}</code>
                <p v-if="disk.error" class="failure">{{ disk.error }}</p>
              </div>
              <div class="capacity">
                <b>{{ bytes(disk.available) }} 可用</b
                ><span>共 {{ bytes(disk.total) }}</span>
              </div>
              <n-tag :type="disk.error ? 'error' : 'success'" size="small">{{
                disk.error ? "异常" : "可访问"
              }}</n-tag>
            </div>
          </section>
          <section class="panel">
            <div class="panel-title">
              <h2>归档目标</h2>
              <span>自动分配 · 实际任务验证</span>
            </div>
            <p v-if="!status?.archives?.length">尚未配置 WebDAV 目标。</p>
            <div v-for="name in status?.archives" :key="name" class="disk">
              <b>{{ name }}</b
              ><n-tag size="small">已配置</n-tag>
            </div>
          </section>
          <section class="panel">
            <div class="panel-title">
              <h2>后台维护</h2>
              <span>每 5 秒刷新</span>
            </div>
            <p v-if="!status?.last">
              等待第一轮维护结果，尚未确认文件保护状态。
            </p>
            <div
              v-for="kind in ['replicate', 'audit', 'archive_audit']"
              :key="kind"
            >
              <div class="task">
                <b>{{
                  kind === "archive_audit"
                    ? "远端校验"
                    : kind === "audit"
                      ? "本地校验"
                      : "副本维护"
                }}</b
                ><span>{{ date(status?.history?.[kind]?.time) }}</span>
              </div>
              <template v-if="status?.history?.[kind]"
                ><div class="task-counts">
                  <span
                    >完成
                    <b>{{ status.history[kind].details.completed }}</b></span
                  ><span
                    >忙碌 <b>{{ status.history[kind].details.busy }}</b></span
                  ><span
                    >错误
                    <b>{{
                      status.history[kind].details.errors?.length
                    }}</b></span
                  >
                </div>
                <p
                  v-for="item in status.history[kind].details.errors"
                  class="failure"
                >
                  {{ item }}
                </p></template
              ><br />
            </div>
            <p v-if="status?.last?.details?.message" class="failure">
              {{ status.last.details.message }}
            </p>
          </section>
          <section class="panel">
            <div class="panel-title">
              <h2>存储维护</h2>
              <span>操作前确认磁盘挂载与 SMB 访问状态</span>
            </div>
            <p>
              恢复仅使用已校验的副本。目标必须为空，失败后保留目标用于检查。
            </p>
            <div class="task-counts">
              <n-button :loading="busy" @click="maintenance('init')"
                >初始化存储</n-button
              ><n-button :loading="busy" @click="maintenance('scan')"
                >立即扫描</n-button
              ><n-button :loading="busy" @click="maintenance('recover-primary')"
                >恢复主盘</n-button
              >
            </div>
            <div class="task-counts">
              <n-button
                v-for="disk in status?.disks?.filter((d) => d.name !== '主盘')"
                :key="disk.name"
                :loading="busy"
                @click="maintenance('replace-replica', disk.name)"
                >重建 {{ disk.name }}</n-button
              >
            </div>
          </section> </template
        ><template v-else
          ><div class="heading">
            <div>
              <div class="eyebrow">CONFIGURATION</div>
              <h1>按你的方式，保存。</h1>
              <p>直接编辑本地 TOML 配置，保存前自动校验。</p>
            </div>
            <n-tag v-if="dirty" type="warning">未保存</n-tag>
          </div>
          <section class="panel editor">
            <div class="panel-title">
              <h2>config.toml</h2>
              <span>保留注释与原有格式</span>
            </div>
            <n-input
              v-model:value="content"
              type="textarea"
              :autosize="{ minRows: 22, maxRows: 40 }"
              spellcheck="false"
              aria-label="TOML 配置内容"
            />
            <div class="editor-footer">
              <span>外部修改会触发冲突提示，请保留草稿后重新打开页面。</span>
              <div>
                <n-button :loading="busy" @click="save(true)">校验配置</n-button
                ><n-button
                  type="primary"
                  :loading="busy"
                  :disabled="!dirty"
                  @click="save(false)"
                  >保存配置</n-button
                >
              </div>
            </div>
          </section></template
        >
        <footer>
          Tier · 让存储回归简单<span>健康状态不等同于完整数据校验</span>
        </footer>
      </main>
    </div></n-config-provider
  >
</template>
