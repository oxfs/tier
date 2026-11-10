import assert from "node:assert/strict";
import { test } from "node:test";
import { forms, initialValue, serialize, policyInput } from "../src/forms.ts";

test("optional counts and empty pools retain their API meaning", () => {
  const fields = Object.fromEntries(forms.policy.map((f) => [f.key, f]));
  assert.equal(serialize(fields.persist_delay_secs, "0"), 0);
  assert.deepEqual(serialize(fields.write_cache, ""), []);
  assert.deepEqual(serialize(fields.write_cache, "disk1, disk2"), [
    "disk1",
    "disk2",
  ]);
  assert.equal(initialValue(fields.persist_delay_secs, 0), 0);
});

test("editing policies preserves explicit checkboxes and previous selections", () => {
  const repairMode = forms.policy.find((f) => f.key === "manual_repair");
  const devices = forms.archive_policy.find((f) => f.key === "targets");
  assert.equal(initialValue(repairMode, true), true);
  assert.equal(serialize(repairMode, false), false);
  assert.deepEqual(serialize(devices, initialValue(devices, ["a", "b"])), [
    "a",
    "b",
  ]);
});

test("a new policy writes directly to persistent storage and can opt into cache", () => {
  const values = Object.fromEntries(
    forms.policy.map((f) => [f.key, initialValue(f, undefined)]),
  );
  values.persistent_storage = "disk";
  const direct = policyInput(values);
  assert.equal(direct.persistent_copies, 1);
  assert.equal(direct.persist_delay_secs, 1800);
  assert.deepEqual(direct.write_cache, []);
  values.write_cache = "ssd";
  assert.deepEqual(policyInput(values).write_cache, ["ssd"]);
  values.write_cache = "";
  values.direct_fallback = true;
  assert.equal(policyInput(values).direct_fallback, false);
});

test("backup plans preserve their kind and disabled state, and inspection can use the latest backup", () => {
  const fields = Object.fromEntries(
    forms.backup_schedule.map((f) => [f.key, f]),
  );
  assert.equal(initialValue(fields.kind, "metadata"), "metadata");
  assert.equal(serialize(fields.kind, "export"), "export");
  assert.equal(serialize(fields.enabled, false), false);
  const id = forms.inspect.find((f) => f.key === "backup_id");
  assert.equal(serialize(id, initialValue(id, undefined)), null);
  assert.equal(serialize(id, "7"), 7);
});
