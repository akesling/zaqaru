// Checks `symbols.js` under Node: node --test web/symbols.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { definitionsOf, readIndex, search } from "./symbols.js";

const index = readIndex({
  files: ["/hubris/sys/kern/src/task.rs", "/hubris/sys/kern/src/umem.rs", "/hubris/drv/i2c/src/lib.rs", "task/doom/doomgeneric/r_draw.c", "task/doom/doomgeneric/r_draw.h"],
  symbols: [
    ["Task", "type", 0, 40, null, 0],
    ["new", "function", 0, 60, "kern::task::Task::new", 0],
    ["new", "function", 2, 12, "drv_i2c::I2cController::new", 0],
    ["safe_copy", "function", 1, 300, "kern::umem::safe_copy", 0],
    ["R_DrawColumn", "function", 4, 31, null, 1],
    ["R_DrawColumn", "function", 3, 120, null, 0],
    ["TaskId", "type", 2, 9, null, 0],
  ],
});

test("a name's definitions, the likeliest first: defined, in the same file, in the nearest directory", () => {
  assert.deepEqual(definitionsOf(index, "R_DrawColumn", "task/doom/doomgeneric/r_main.c").map((d) => d.line), [120, 31]);
  assert.deepEqual(definitionsOf(index, "new", "/hubris/sys/kern/src/umem.rs").map((d) => d.path), ["kern::task::Task::new", "drv_i2c::I2cController::new"]);
  assert.deepEqual(definitionsOf(index, "new", "/hubris/drv/i2c/src/lib.rs").map((d) => d.path), ["drv_i2c::I2cController::new", "kern::task::Task::new"]);
  assert.deepEqual(definitionsOf(index, "nothing", "a.rs"), []);
});

test("symbols found by name — exactly, its start, within it — or by path; only declared left out when defined", () => {
  assert.deepEqual(search(index, "task").map((s) => s.name), ["Task", "TaskId"]);
  assert.deepEqual(search(index, "Task::new").map((s) => s.path), ["kern::task::Task::new"]);
  assert.deepEqual(search(index, "copy").map((s) => s.name), ["safe_copy"]);
  assert.deepEqual(search(index, "R_Draw").map((s) => `${s.file}:${s.line}`), ["task/doom/doomgeneric/r_draw.c:120"]);
  assert.deepEqual(search(index, " "), []);
});
