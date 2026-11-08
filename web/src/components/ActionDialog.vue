<script setup lang="ts">
import { ref, watch, nextTick } from "vue";
import {
  forms,
  hasCache,
  policyInput,
  initialValue,
  serialize,
  type Input,
  type Field,
} from "../forms";
import { actions, type State } from "../model";
const props = defineProps<{
  action: string;
  state?: State;
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
      const previous = props.initial[field.key];
      values.value[field.key] = initialValue(
        field,
        field.key === "cache_copies" && previous === 0 ? undefined : previous,
      );
    }
    await nextTick();
    if (!modal.value?.open) modal.value?.showModal();
  },
  { immediate: true },
);
function options(key: string) {
  if (key === "targets") return props.state?.cloud_targets ?? [];
  if (key === "write_cache" || key === "persistent_storage")
    return props.state?.targets ?? [];
}
function selected(field: Field): string[] {
  return serialize(field, values.value[field.key]) as string[];
}
function toggle(field: Field, name: string) {
  const names = selected(field);
  values.value[field.key] = (
    names.includes(name) ? names.filter((n) => n !== name) : [...names, name]
  ).join(", ");
}
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
    const input =
      props.action === "policy"
        ? policyInput(values.value)
        : Object.fromEntries(
            forms[props.action].map((field) => [
              field.key,
              serialize(field, values.value[field.key]),
            ]),
          );

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
        <template v-for="field in forms[action]" :key="field.key">
          <h3
            class="form-section"
            v-if="field.section && (!field.cacheOnly || hasCache(values))"
          >
            {{ field.section }}
          </h3>
          <div
            v-if="
              (!field.cacheOnly || hasCache(values)) &&
              (field.key !== 'fallback_count' || values.direct_fallback)
            "
            :class="{
              field: true,
              wide: field.kind !== 'number',
            }"
          >
            <label :for="field.key">{{ field.label }}</label>
            <details v-if="field.hint" class="field-help">
              <summary :aria-label="field.label + '说明'" :title="field.hint">
                ?
              </summary>
              <small :id="field.key + '-hint'">{{ field.hint }}</small>
            </details>
            <input
              v-if="field.kind === 'checkbox'"
              type="checkbox"
              :id="field.key"
              :aria-describedby="field.hint ? field.key + '-hint' : undefined"
              v-model="values[field.key]"
            />
            <input
              v-else
              :id="field.key"
              :aria-describedby="field.hint ? field.key + '-hint' : undefined"
              v-model="values[field.key]"
              :type="field.kind === 'number' ? 'number' : 'text'"
              :required="!field.optional"
              :min="field.min"
              :max="field.max"
              :placeholder="field.placeholder"
            />
            <div v-if="options(field.key)" class="target-options">
              <button
                v-for="item in options(field.key)"
                type="button"
                :key="item.name"
                :aria-pressed="selected(field).includes(item.name)"
                @click="toggle(field, item.name)"
              >
                {{ item.name }}
              </button>
              <small v-if="!options(field.key)?.length">
                {{
                  field.key === "targets"
                    ? "请先在「归档」添加目标。"
                    : "请先在「存储设备」登记目标。"
                }}
              </small>
            </div>
          </div>
        </template>
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
