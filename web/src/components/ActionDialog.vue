<script setup lang="ts">
import { ref, watch, nextTick } from "vue";
import { forms, initialValue, serialize, type Input } from "../forms";
import { actions } from "../model";
const props = defineProps<{
  action: string;
  initial: Record<string, unknown>;
}>();
const emit = defineEmits<{ close: []; updated: [] }>();
const modal = ref<HTMLDialogElement>(),
  values = ref<Record<string, Input>>({}),
  busy = ref(false),
  error = ref(""),
  report = ref<unknown>();

watch(
  () => props.action,
  async (action) => {
    if (!action) return;
    values.value = {};
    error.value = "";
    report.value = undefined;
    for (const field of forms[action]) {
      values.value[field.key] = initialValue(field, props.initial[field.key]);
    }
    await nextTick();
    if (!modal.value?.open) modal.value?.showModal();
  },
  { immediate: true },
);
function close() {
  if (busy.value) return;
  modal.value?.close();
  emit("close");
}
async function submit() {
  if (
    props.action === "purge" &&
    !confirm("永久删除此回收条目及其可清理的副本？")
  )
    return;
  busy.value = true;
  error.value = "";
  try {
    const input: Record<string, unknown> = {};
    for (const field of forms[props.action]) {
      input[field.key] = serialize(field, values.value[field.key]);
    }
    const response = await fetch("/api/action", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ action: props.action, input }),
    });
    const result = await response.json();
    if (!response.ok) throw Error(result.error ?? "操作失败");
    emit("updated");
    if (props.action === "inspect") report.value = result;
    else {
      modal.value?.close();
      emit("close");
    }
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}
</script>
<template>
  <dialog ref="modal" aria-labelledby="dlg-title" @cancel.prevent="close">
    <form @submit.prevent="submit">
      <div class="dialog-head">
        <h2 id="dlg-title">{{ actions[action] }}</h2>
        <button type="button" aria-label="关闭" @click="close">×</button>
      </div>
      <div id="fields">
        <label
          v-for="field in forms[action]"
          :key="field.key"
          :class="{
            check: field.kind === 'checkbox',
            wide: field.kind !== 'number',
          }"
        >
          {{ field.label }}
          <input
            v-if="field.kind === 'checkbox'"
            type="checkbox"
            v-model="values[field.key]"
          />
          <input
            v-else
            v-model="values[field.key]"
            :type="field.kind === 'number' ? 'number' : 'text'"
            :required="!field.optional"
            :min="field.min"
            :max="field.max"
            :placeholder="field.placeholder"
          />
        </label>
      </div>
      <p v-if="action === 'repair'" class="muted">
        检查当前文件并修复可恢复的副本。
      </p>
      <pre v-if="report">{{ JSON.stringify(report, null, 2) }}</pre>
      <p v-if="error" id="feedback" role="alert">{{ error }}</p>
      <footer>
        <button type="button" @click="close">关闭</button>
        <button :disabled="busy" class="primary">
          {{ busy ? "处理中…" : actions[action] }}
        </button>
      </footer>
    </form>
  </dialog>
</template>
