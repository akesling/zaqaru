// Checks `derive.js` under Node: node --test web/derive.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { WINDOW } from "./trail.js";
import { addressExpression, isAddress, deriveField, expression, goOn, nodesOf, onwards, operation, readTree, recurrence, valueText } from "./derive.js";

const P = 0x0808_7aac;
const LEVELTIME = 0x2401_7458;

/// `leveltime`'s store of `value`, made of what it held before, written
/// at `stamp` by the same code.
function tic(value, stamp) {
  return {
    tag: 9,
    role: "value",
    step: { what: "store", addr: LEVELTIME, size: 4, value, pc: P, t: stamp * 1000 - 500 },
    operands: [{ tag: 3, role: "value", step: { what: "held", addr: LEVELTIME, size: 4, value: value - 1, ram: true, written: { stamp: stamp - 28_000, by: { pc: P } }, pc: P }, operands: [] }],
  };
}

test("an instruction's operation is said with a symbol, or in words", () => {
  assert.deepEqual(operation("add"), { symbol: "+", word: "added" });
  assert.equal(operation("ubfx").symbol, null);
  assert.equal(expression("add", [0xa38000, 0x8000]), "0xa38000 + 0x8000");
  assert.equal(expression("lsl", [0x10]), "shifted left (0x10)", "one operand followed, its constant not known");
  assert.equal(expression("lsr", [0x10c492e8], { imm: 10 }), "0x10c492e8 >> 10");
  assert.equal(expression("and", [0x4312], { imm: 0xfc0 }), "0x4312 & 0xfc0");
  assert.equal(expression("add", [0x2c4], { imm: 1 }), "0x2c4 + 1");
  assert.equal(expression("ubfx", [0x1f3], { bits: [4, 6] }), "bits 4–9 of 0x1f3");
  assert.equal(expression("add", []), null, "said above: nothing to say it of");
  assert.equal(expression("add", [0x903cd7b4, 0xbb418e45], { value: 0x903cd7e2, rshift: ["lsr", 26] }), "0x903cd7b4 + 0xbb418e45 >> 26", "the shifted one told by what it comes to");
  assert.equal(expression("add", [0xbb418e45, 0x903cd7b4], { value: 0x903cd7e2, rshift: ["lsr", 26] }), "0x903cd7b4 + 0xbb418e45 >> 26");
  assert.equal(expression("add", [0xbb418e45], { value: 0x903cd7e2, rshift: ["lsr", 26] }, true), "0x903cd7b4 + 0xbb418e45 >> 26", "a pointer left out: from what it came to");
  assert.equal(expression("orr", [0x1, 0x2], { value: 0x201, rshift: ["lsl", 8] }), "0x1 | 0x2 << 8");
  assert.equal(expression("and", [0x6b448cb], { value: 0x480, rshift: ["lsr", 4] }), "0x6b448cb >> 4 & a constant", "a mask the code made before");
  assert.equal(expression("weird", [1, 2]), "weird (0x1, 0x2)");
  assert.equal(valueText(3), "3");
  assert.equal(valueText(0x2ce), "0x2ce (718)");
  assert.equal(valueText(0xffffffff), "0xffffffff (-1)");
  assert.equal(valueText(0x2401_7458), "0x24017458");
});

test("a tree is read with each node's parent and window, and goes on where it can", () => {
  const nodes = new Map();
  const root = readTree(tic(0x2cf, 29_274_000), { time: 29_274e6, from: 29_273e6 }, nodes);
  assert.equal(nodes.size, 2);
  const held = root.operands[0];
  assert.equal(held.parent, root);
  assert.deepEqual(onwards(held), { path: `board/derive/${LEVELTIME}`, time: 29_246_000e6, from: 29_246_000e6 - WINDOW });
  assert.equal(onwards(root), null, "made of something already");
  assert.equal(recurrence(held), root, "its own value, before");
  const register = readTree({ tag: 4, step: { what: "register", register: "r7", value: 5, t: 1_000_000 }, operands: [] }, null);
  assert.deepEqual(onwards(register), { path: "board/derive/r7", time: 1e9, from: 1e9 - WINDOW, value: 5 });
  const cut = readTree({ tag: 77, cut: true, step: { what: "merge" }, operands: [] }, { time: 5, from: 4 });
  assert.deepEqual(onwards(cut), { path: "board/derive/n77", time: 5, from: 4 });
});

test("a count made a window at a time is folded into how many times", async () => {
  const nodes = new Map();
  let stamp = 29_274_000;
  const root = readTree(tic(0x2cf, stamp), { time: stamp * 1e6, from: stamp * 1e6 - WINDOW }, nodes);
  const asked = [];
  // Each window back, the tic before; the fourth back, held since reset.
  const query = async (options, path) => {
    asked.push([options.time, path]);
    stamp -= 28_000;
    const tree = tic(0x2cf - asked.length, stamp);
    if (asked.length === 3) tree.operands[0].step.written = null;
    return { tree };
  };
  await goOn(root.operands[0], { query, nodes });
  assert.equal(asked.length, 3, "three windows back, then held since before anything wrote it");
  assert.equal(root.repeats, 3, "three times before this one");
  assert.equal(root.operands.length, 1);
  const before = root.operands[0];
  assert.equal(before.role, "before");
  assert.equal(before.step.what, "held");
  assert.equal(before.step.value, 0x2cf - 4, "what it began from");
  assert.equal(onwards(before), null, "and no further");
});

test("what was found that is no fold is what the node was made of", async () => {
  const nodes = new Map();
  const root = readTree(
    { tag: 1, step: { what: "store", addr: 0x2400_0000, size: 4, value: 9, pc: 0x0800_0100 }, operands: [{ tag: 2, step: { what: "held", addr: 0x2400_0100, size: 4, value: 9, ram: true, written: { stamp: 7, by: { pc: 0x0800_0200 } } }, operands: [] }] },
    { time: 1e7, from: 0 },
    nodes,
  );
  const query = async () => ({ tree: { tag: 5, step: { what: "store", addr: 0x2400_0100, size: 4, value: 9, pc: 0x0800_0200 }, operands: [] } });
  await goOn(root.operands[0], { query, nodes });
  const held = root.operands[0];
  assert.equal(held.operands.length, 1);
  assert.equal(held.operands[0].step.pc, 0x0800_0200, "the store that put it there");
  assert.equal(root.repeats, 0);
  assert.deepEqual(
    deriveField(root).map((p) => p.addr),
    [0x2400_0000, 0x2400_0100, 0x2400_0100],
  );
  assert.equal(nodesOf(root).length, 3);
});

test("a register that held other than it was found holding is lost there", async () => {
  const node = readTree({ tag: 4, step: { what: "register", register: "r7", value: 5, t: 1_000_000 }, operands: [] }, null);
  await goOn(node, { query: async () => ({ value: 6, tree: null }) });
  assert.deepEqual(node.lost, { expected: 5, found: 6 });
});

test("a count folded in its window and then across them says what it began from once", async () => {
  const nodes = new Map();
  // Three tics in the window: the earliest made of what was held before.
  const root = readTree(
    {
      tag: 9,
      step: { what: "store", addr: LEVELTIME, size: 4, value: 0x2cf, pc: P },
      repeats: 2,
      earliest: { what: "store", addr: LEVELTIME, size: 4, value: 0x2cd, pc: P, t: 10 },
      operands: [{ tag: 7, role: "before", step: { what: "store", addr: LEVELTIME, size: 4, value: 0x2cd, pc: P }, operands: [tic(0x2cd, 100).operands[0]] }],
    },
    { time: 1, from: 0 },
    nodes,
  );
  const held = root.operands[0].operands[0];
  assert.equal(recurrence(held), root);
  let n = 0;
  const query = async () => {
    n++;
    const tree = tic(0x2cc, 50);
    tree.operands[0].step.written = null;
    return { tree };
  };
  await goOn(held, { query, nodes });
  assert.equal(n, 1);
  assert.equal(root.repeats, 3);
  assert.equal(root.operands.length, 1);
  const before = root.operands[0];
  assert.equal(before.role, "before");
  assert.equal(before.step.what, "held", "said once, not inside the window's own");
  assert.equal(before.step.value, 0x2cb);
});

test("an address is said as its base and its index, shifted", () => {
  assert.equal(addressExpression(0x2406ad84 + (0x60 << 2), [0x60, 0x2406ad84], 2), "0x2406ad84 + 0x60 << 2", "told apart by which way round comes to it");
  assert.equal(addressExpression(0x2400_0105, [0x2400_0100, 5], 0), "0x24000100 + 0x5");
  assert.equal(addressExpression(0x903ce0a2, [0x8c0], 0), "0x903cd7e2 + 0x8c0", "the table's own left out: taken from the address");
  assert.equal(addressExpression(0x2406ad84 + (0x60 << 2), [0x60], 2), "0x2406ad84 + 0x60 << 2");
  assert.ok(isAddress({ what: "merge", op: "ldr" }));
  assert.ok(!isAddress({ what: "merge", op: "add" }));
});
