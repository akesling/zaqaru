// Checks `trail.js` under Node: node --test web/trail.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { WINDOW, byteOf, folded, follow, functionOf, isPlumbing, leafText, next, shortPlace, ownerOf, placeOwner, span, summary } from "./trail.js";

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

const drawn = { what: "store", addr: 0x2400_8080, size: 1, value: 0xbf, byte: 0x2400_8080, where: "doom .uninit: task_doom::ZONE_AXI + 0x8080 (AXI SRAM + 0x5941c)", code: "doom: V_DrawPatch + 0x12", lines: [{ function: "V_DrawPatch", file: "doomgeneric/v_video.c", line: 189 }], t: 43105713000 };
const copied = { what: "store", addr: 0x3800_0028, size: 4, value: 0x4f4fbfbf, byte: 0x3800_0028, where: "display .frame: drv_stm32h7_ltdc::FRAME_SHOWN + 0x28 (SRAM4 + 0x28)", code: "kernel: compiler_builtins::mem::impls::copy_forward_aligned_words + 0x8", lines: [{ function: "compiler_builtins::mem::impls::copy_forward_aligned_words", file: "mem/impls.rs", line: 130 }], t: 43105732000 };
const loadedForCopy = { ...copied, what: "held", where: drawn.where, addr: drawn.addr, byte: drawn.addr };
const fromWad = { what: "held", ram: false, addr: 0x9000_1000, size: 1, value: 0xbf, byte: 0x9000_1000, where: "the NOR: WAD lump STCFN068 + 0x5b", code: "doom: V_DrawPatch + 0x10", lines: drawn.lines, t: 43105713000 };

test("plumbing is told apart from the code that makes a value", () => {
  assert.ok(isPlumbing(copied) && isPlumbing(loadedForCopy));
  assert.ok(!isPlumbing(drawn) && !isPlumbing(fromWad));
  assert.equal(functionOf(copied), "copy_forward_aligned_words");
  assert.equal(functionOf({ lines: [{ function: "<drv::I2c as drv::Read>::read::{closure#1}>", line: 764 }] }), "{closure#1}");
  assert.equal(ownerOf(drawn), "doom");
  assert.equal(placeOwner(copied), "display");
  assert.equal(placeOwner(fromWad), null);
  assert.equal(placeOwner({ where: "i2c_driver stack (AXI SRAM + 0xd3ea)" }), "i2c_driver");
  assert.equal(byteOf(copied), 0xbf);
});

test("a run of copies folds into one, the rest stays as it is", () => {
  const steps = [copied, loadedForCopy, drawn, fromWad].map((step) => ({ step, hop: 0 }));
  const shown = folded(steps);
  assert.equal(shown.length, 3);
  assert.equal(shown[0].copies.length, 2);
  assert.equal(shown[1].step, drawn);
});

test("a story says in a sentence where its value came from, what made it and how it got here", () => {
  const trail = { hops: [{ steps: [copied, loadedForCopy, drawn, fromWad] }], end: { kind: "rom", step: fromWad } };
  assert.equal(
    summary(trail),
    "0xbf came from the NOR: WAD lump STCFN068 + 0x5b, was stored by V_DrawPatch (v_video.c:189) in doom, and was copied once into display, 19 µs later.",
  );
  assert.equal(span(19e6), "19 µs");
  assert.equal(span(2.5e9), "2.5 ms");
});

test("a value looked up in a table says what chose it", () => {
  const texel = { what: "held", ram: true, addr: 0x2400_9000, size: 1, value: 0x6a, byte: 0x2400_9000, where: "doom .uninit: ZONE_AXI[36864] (AXI SRAM + 0x5a3a4)" };
  const lookup = { what: "load", addr: 0x9000_1503, byte: 0x9000_1503, where: "the NOR: WAD lump COLORMAP + 0x503", address: [texel], code: drawn.code, lines: drawn.lines, t: drawn.t };
  const entry = { ...fromWad, where: "the NOR: WAD lump COLORMAP + 0x503: light map 5, colour 0x03" };
  assert.equal(leafText(texel), "0x6a from ZONE_AXI[36864]");
  const trail = { hops: [{ steps: [copied, drawn, lookup, entry] }], end: { kind: "rom", step: entry } };
  assert.match(summary(trail), /was stored by V_DrawPatch \(v_video.c:189\) in doom, looked up by 0x6a from ZONE_AXI\[36864\], and was copied once/);
});

test("a place is said short, keeping what its lump's format says", () => {
  assert.equal(shortPlace("doom .uninit: task_doom::ZONE_AXI[55416] (AXI SRAM + 0x5941c)"), "ZONE_AXI[55416]");
  assert.equal(shortPlace("the NOR: WAD lump SW11_2 + 0xa7a: picture 32×128, pixel (12, 70)"), "the NOR: WAD lump SW11_2 + 0xa7a: picture 32×128, pixel (12, 70)");
  assert.equal(shortPlace("i2c_driver stack (AXI SRAM + 0xd3ea)"), "i2c_driver stack");
});
