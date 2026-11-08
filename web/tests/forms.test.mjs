import assert from "node:assert/strict";
import { test } from "node:test";
import { forms, initialValue, serialize } from "../src/forms.ts";

test("optional counts and empty pools retain their API meaning", () => {
  const fields = Object.fromEntries(forms.policy.map((f) => [f.key, f]));
  assert.equal(serialize(fields.fallback_count, ""), null);
  assert.equal(serialize(fields.long_count, "0"), 0);
  assert.deepEqual(serialize(fields.long, ""), []);
  assert.deepEqual(serialize(fields.long, "disk1, disk2"), ["disk1", "disk2"]);
  assert.equal(initialValue(fields.long_count, 0), 0);
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
