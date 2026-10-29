// Checks `checkpoints.js`'s forgetting under Node: node --test web/checkpoints.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { Checkpoints, PAGE } from "./checkpoints.js";

/// A stand-in container: its memory, and nothing that tracks writes.
const container = (memory) => ({ memory: { buffer: memory.buffer }, written: () => null, stackPointer: 0, mounts: { snapshot: () => null } });

test("a checkpoint forgotten leaves every other one restoring byte for byte", () => {
  const store = new Checkpoints({ fullEvery: 3 });
  const memory = new Uint8Array(6 * PAGE);
  const images = [];
  for (let i = 0; i < 8; i++) {
    memory[(i % 6) * PAGE + i] = i + 1; // a page changed each time
    if (i % 3 === 0) memory[5 * PAGE] = 100 + i; // and one again and again
    store.add(i * 10, container(memory));
    images.push(memory.slice());
  }
  for (const index of [3, 1, 4]) {
    assert.ok(store.drop(index));
    images.splice(index, 1);
  }
  assert.equal(store.length, images.length);
  images.forEach((image, i) => assert.deepEqual(store.memory(i), image, `checkpoint ${i}`));
  assert.ok(!store.drop(0) && !store.drop(store.length - 1), "the first and the last are kept");
});

test("of checkpoints at one count, the first is the one before it", () => {
  const store = new Checkpoints();
  const memory = new Uint8Array(PAGE);
  for (const at of [0, 50, 50, 50, 90]) store.add(at, container(memory));
  assert.equal(store.before(60), 1);
  assert.equal(store.before(50), 1);
  assert.equal(store.before(95), 4);
});
