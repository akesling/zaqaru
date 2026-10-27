// Checks `memory.js` under Node: node --test web/memory.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { cellOwners, heat, hilbert, layout, owners, rounded, shortName, tiles } from "./memory.js";

test("a Hilbert curve visits every point of its square once, each next to the last", () => {
  const seen = new Set();
  let last = null;
  for (let d = 0; d < 64; d++) {
    const [x, y] = hilbert(8, d);
    seen.add(`${x},${y}`);
    if (last) assert.equal(Math.abs(x - last[0]) + Math.abs(y - last[1]), 1);
    last = [x, y];
  }
  assert.equal(seen.size, 64);
});

test("a RAM is tiled in powers of two, largest first", () => {
  // 288 KiB of 64-byte cells: 4096 and 512.
  assert.deepEqual(tiles(4608).map((t) => [t.count, t.width, t.height]), [[4096, 64, 64], [512, 32, 16]]);
  assert.deepEqual(tiles(8192).map((t) => [t.width, t.height]), [[128, 64]]);
});

test("every cell of a picture has a place of its own, and each place its cell", () => {
  const view = layout(288 * 1024);
  assert.equal(view.cells, 4608);
  const places = new Set();
  for (let c = 0; c < view.cells; c++) {
    const [x, y] = [view.place[2 * c], view.place[2 * c + 1]];
    places.add(y * view.width + x);
    assert.equal(view.at[y * view.width + x], c);
  }
  assert.equal(places.size, view.cells);
});

test("an aligned power-of-two region is one block of the picture", () => {
  // 4 KiB at 8 KiB into a 64 KiB RAM: 64 cells, an 8×8 square.
  const view = layout(65536);
  const xs = [];
  const ys = [];
  for (let c = 128; c < 192; c++) {
    xs.push(view.place[2 * c]);
    ys.push(view.place[2 * c + 1]);
  }
  assert.equal(Math.max(...xs) - Math.min(...xs), 7);
  assert.equal(Math.max(...ys) - Math.min(...ys), 7);
});

test("each cell is its owner's by the most of its bytes, and a stack is marked", () => {
  const tasks = {
    kernel: { name: "kernel", index: null, stack: { base: 0x1000, size: 0x100 }, sections: [{ name: ".bss", base: 0x1100, size: 0x20, writable: true }] },
    tasks: [{ name: "jefe", index: 0, stack: null, sections: [{ name: ".bss", base: 0x1118, size: 0x60, writable: true }, { name: ".text", base: 0, size: 4, writable: false }] }],
  };
  const list = owners(tasks, (n) => `hsl(${n} 50% 50%)`);
  assert.deepEqual(list.map((o) => o.ranges.length), [2, 1]);
  const { owner, stack } = cellOwners(0x1000, layout(0x200), list);
  assert.deepEqual([...owner.slice(0, 7)], [0, 0, 0, 0, 1, 1, -1]);
  assert.deepEqual([...stack.slice(0, 5)], [1, 1, 1, 1, 0]);
});

test("heat halves every half-life", () => {
  assert.equal(heat(0, 100), 1);
  assert.equal(heat(100, 100), 0.5);
  assert.equal(heat(300, 100), 0.125);
});

test("a static's name is shortened to its last two parts, generics folded", () => {
  assert.equal(shortName("drv_stm32xx_i2c_server::__RINGBUF"), "drv_stm32xx_i2c_server::__RINGBUF");
  assert.equal(shortName("a::b::c::D"), "c::D");
  assert.equal(shortName("<x::Y<u8> as z::W>::F"), "<…>::F");
});

test("a finer picture is the coarser one refined: each cell a block of the finer's in its place", () => {
  // 288 KiB — two tiles — at 64, 16 and 4 bytes a cell.
  for (const [coarse, fine, f] of [[64, 16, 2], [16, 4, 2], [64, 4, 4]]) {
    const a = layout(288 * 1024, coarse);
    const b = layout(288 * 1024, fine);
    assert.equal(b.width, a.width * f);
    assert.equal(b.height, a.height * f);
    for (let c = 0; c < b.cells; c += 7) {
      const parent = Math.floor((c * fine) / coarse);
      assert.equal(Math.floor(b.place[2 * c] / f), a.place[2 * parent]);
      assert.equal(Math.floor(b.place[2 * c + 1] / f), a.place[2 * parent + 1]);
    }
  }
});

test("a run rounds out to the aligned block that holds it, within its RAM", () => {
  // hiffy's 32 KiB, less its first cell: the whole of it.
  assert.deepEqual(rounded(0x24010040, 0x7fc0), { base: 0x24010000, length: 0x8000 });
  // Across a 64 KiB boundary: the block that holds both sides.
  assert.deepEqual(rounded(0x2400f000, 0x2000), { base: 0x24000000, length: 0x20000 });
  // A block past the end of the RAM is no block: the run as it is.
  const ram = { base: 0x38800000, length: 4096 };
  assert.deepEqual(rounded(0x38800e00, 0x180, ram), { base: 0x38800e00, length: 0x200 });
  assert.deepEqual(rounded(0x387fff00, 0x200, ram), { base: 0x387fff00, length: 0x200 });
});
