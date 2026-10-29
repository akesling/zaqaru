// Checks `trail.js` under Node: node --test web/trail.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { WINDOW, follow, next } from "./trail.js";

const by = { pc: 0x0800_1234, code: "doom: R_DrawColumn" };
const held = (addr, stamp, writer = by) => ({ what: "held", addr, ram: true, written: stamp ? { stamp, by: writer } : null });

test("a history's last step says where to go on, or that it ends", () => {
  assert.deepEqual(next(held(0x2400_0010, 7)), { at: 0x2400_0010, time: 7e6 });
  assert.deepEqual(next({ ...held(0x2400_0010, 7), byte: 0x2400_0012 }), { at: 0x2400_0012, time: 7e6 });
  assert.deepEqual(next(held(0x2400_0010, 0)), { end: "unwritten" });
  assert.deepEqual(next(held(0x2400_0010, 7, { code: "DMA" })), { end: "writer" });
  assert.deepEqual(next({ what: "held", addr: 0x9000_0000, ram: false }), { end: "rom" });
  assert.deepEqual(next({ what: "read" }), { end: "bus" });
  assert.deepEqual(next({ what: "store" }), { end: "constant" });
  assert.deepEqual(next({ what: "register" }), { widen: true });
  assert.deepEqual(next(undefined), { end: "unfollowed" });
});

test("a story hops from write to write, widening for a register, to where the value came in", async () => {
  const asked = [];
  const answers = {
    // The frame's byte: copied by the kernel from doom's buffer.
    [`${30e9} 2400`]: [{ what: "store" }, held(0x2400_0100, 20000)],
    // Doom's buffer: from its colormap, which sat in a register at first.
    [`${20e9} 24000100`]: [{ what: "store" }, { what: "register" }],
    [`${20e9} 24000100 wide`]: [{ what: "store" }, held(0x2400_0200, 5000)],
    // The colormap: out of the WAD.
    [`${5e9} 24000200`]: [{ what: "store" }, { what: "held", addr: 0x9000_1000, ram: false }],
  };
  const query = async ({ time, follow: { from } }, path) => {
    const at = Number(path.split("/").at(-1)).toString(16);
    asked.push([time, time - from]);
    const wide = time - from > WINDOW ? " wide" : "";
    return { what: at, steps: answers[`${time} ${at}${wide}`] ?? answers[`${time} ${at}`] };
  };
  const read = async () => ({ words: [30000, 0], writers: [by] });
  const trail = { address: 0x2400, hops: [] };
  await follow(trail, { read, query });
  assert.deepEqual(
    trail.hops.map((h) => [h.at, h.time]),
    [
      [0x2400, 30e9],
      [0x2400_0100, 20e9],
      [0x2400_0200, 5e9],
    ],
  );
  assert.equal(trail.end.kind, "rom");
  assert.deepEqual(asked.map(([, w]) => w), [WINDOW, WINDOW, 10 * WINDOW, WINDOW]);
});

test("a story stops after so many windows, and goes on from there", async () => {
  const query = async ({ time }) => ({ steps: [{ what: "store" }, held(0x2400_0000, time / 1e6 - 1)] });
  const read = async () => ({ words: [100, 0], writers: [by] });
  const trail = { address: 0x2400_0000, hops: [] };
  await follow(trail, { read, query, hops: 3 });
  assert.equal(trail.hops.length, 3);
  assert.deepEqual(trail.end, { kind: "more", at: 0x2400_0000, time: 97e6 });
  trail.from = trail.end;
  trail.end = null;
  await follow(trail, { read, query, hops: 2 });
  assert.deepEqual(trail.hops.map((h) => h.time / 1e6), [100, 99, 98, 97, 96]);
});

test("bytes never written, or written by no instruction, end before any window", async () => {
  const query = async () => assert.fail("no window run");
  let trail = { address: 4, hops: [] };
  await follow(trail, { read: async () => ({ words: [0, -1], writers: [] }), query });
  assert.equal(trail.end.kind, "unwritten");
  trail = { address: 4, hops: [] };
  await follow(trail, { read: async () => ({ words: [9, 0], writers: [{ code: "the probe" }] }), query });
  assert.deepEqual(trail.end, { kind: "writer", by: { code: "the probe" } });
});

test("a window run once is not run again", async () => {
  let runs = 0;
  const query = async () => (runs++, { steps: [{ what: "read" }] });
  const read = async () => ({ words: [5000, 0], writers: [by] });
  const cache = new Map();
  for (let i = 0; i < 2; i++) await follow({ address: 8, hops: [] }, { read, query, cache });
  assert.equal(runs, 1);
});
