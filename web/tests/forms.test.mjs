import assert from "node:assert/strict";
import { test } from "node:test";
import { forms, initialValue, serialize, policyInput } from "../src/forms.ts";

test("optional counts and empty pools retain their API meaning", () => {
  const fields = Object.fromEntries(forms.policy.map((f) => [f.key, f]));
  assert.equal(serialize(fields.fallback_count, ""), null);
  assert.equal(serialize(fields.cache_copies, "0"), 0);
  assert.deepEqual(serialize(fields.write_cache, ""), []);
  assert.deepEqual(serialize(fields.write_cache, "disk1, disk2"), [
    "disk1",
    "disk2",
  ]);
  assert.equal(initialValue(fields.cache_copies, 0), 0);
});

test("editing policies preserves explicit checkboxes and previous selections", () => {
  const repairMode = forms.policy.find((f) => f.key === "manual_repair");
  const devices = forms.cloud_policy.find((f) => f.key === "targets");
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
  assert.equal(direct.cache_copies, 0);
  assert.deepEqual(direct.write_cache, []);
  values.write_cache = "ssd";
  assert.equal(policyInput(values).cache_copies, 1);
  values.write_cache = "";
  values.direct_fallback = true;
  assert.equal(policyInput(values).direct_fallback, false);
});
