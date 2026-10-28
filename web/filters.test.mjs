// Checks `filters.js` under Node: node --test web/filters.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { Options, emptySpec, isEmpty, matches, pathGroup } from "./filters.js";

const syscall = { process: "humility", syscall: "read", text: "read(3, …) = 64" };
const exchange = { process: "humility", path: "iso/net/conn/7/data", text: "read /iso/net/conn/7/data → 12 bytes" };
const record = { task: "i2c_driver", call: "send", text: "send → sys op 3" };

test("a row passes when it has every value chosen, and nothing chosen passes all", () => {
  assert.ok(isEmpty(emptySpec()));
  for (const row of [syscall, exchange, record]) assert.ok(matches(emptySpec(), row));
  const humility = { ...emptySpec(), process: "humility" };
  assert.deepEqual([syscall, exchange, record].map((r) => matches(humility, r)), [true, true, false]);
});

test("a field chosen leaves out the rows that have no such field", () => {
  const reads = { ...emptySpec(), syscall: "read" };
  assert.deepEqual([syscall, exchange, record].map((r) => matches(reads, r)), [true, false, false]);
  const task = { ...emptySpec(), task: "i2c_driver" };
  assert.deepEqual([syscall, exchange, record].map((r) => matches(task, r)), [false, false, true]);
});

test("a host path chosen takes in the paths under it; text is matched in any case", () => {
  assert.ok(matches({ ...emptySpec(), path: "iso/net/conn" }, exchange));
  assert.ok(!matches({ ...emptySpec(), path: "iso/net/con" }, exchange));
  assert.ok(matches({ ...emptySpec(), text: "SYS OP" }, record));
  assert.ok(!matches({ ...emptySpec(), text: "recv" }, record));
});

test("paths are grouped up to the segment that names one of many", () => {
  assert.equal(pathGroup("iso/net/conn/7/data"), "iso/net/conn");
  assert.equal(pathGroup("iso/time/monotonic_ns"), "iso/time/monotonic_ns");
  assert.equal(pathGroup("services/usb/devices/3/endpoints"), "services/usb/devices");
});

test("the dropdowns offer what the rows had, most frequent first, keeping what is chosen", () => {
  const options = new Options();
  for (const row of [syscall, syscall, { ...syscall, syscall: "write" }, exchange, record]) options.note(row);
  assert.deepEqual(options.list("syscall"), [["read", 2], ["write", 1]]);
  assert.deepEqual(options.list("path"), [["iso/net/conn", 1]]);
  assert.deepEqual(options.list("process"), [["humility", 4]]);
  assert.deepEqual(options.list("task", "jefe"), [["jefe", 0], ["i2c_driver", 1]]);
});
