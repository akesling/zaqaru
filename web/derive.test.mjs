// Checks `derive.js` under Node: node --test web/derive.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { WINDOW } from "./trail.js";
import { addressExpression, count, operandValues, loopSteps, stepsText, isAddress, deriveField, expression, goOn, nodesOf, onwards, operation, readTree, recurrence, valueText } from "./derive.js";

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
  assert.equal(expression("add", [0xa38000, 0x8000], { value: 0xa40000 }), "0xa38000 + 0x8000");
  assert.equal(expression("add", [0xa38000, 0x8000]), "added (0xa38000, 0x8000)", "nothing to check it against: not said as arithmetic");
  assert.equal(expression("lsl", [0x10], { value: 0x40 }), "shifted left (0x10, a constant)", "one operand followed, its constant not known");
  assert.equal(expression("lsr", [0x10c492e8], { value: 0x10c492e8 >>> 10, imm: 10 }), "0x10c492e8 >> 10");
  assert.equal(expression("and", [0x4312], { value: 0x4312 & 0xfc0, imm: 0xfc0 }), "0x4312 & 0xfc0");
  assert.equal(expression("add", [0x2c4], { value: 0x2c5, imm: 1 }), "0x2c4 + 1");
  assert.equal(expression("ubfx", [0x1f3], { bits: [4, 6] }), "bits 4–9 of 0x1f3");
  assert.equal(expression("add", []), null, "said above: nothing to say it of");
  assert.equal(expression("add", [0x903cd7b4, 0xbb418e45], { value: 0x903cd7e2, rshift: ["lsr", 26] }), "0x903cd7b4 + (0xbb418e45 >> 26)", "the shifted one told by what it comes to, bracketed");
  assert.equal(expression("add", [0xbb418e45, 0x903cd7b4], { value: 0x903cd7e2, rshift: ["lsr", 26] }), "0x903cd7b4 + (0xbb418e45 >> 26)");
  assert.equal(expression("add", [0xbb418e45], { value: 0x903cd7e2, rshift: ["lsr", 26] }, true), "0x903cd7b4 + (0xbb418e45 >> 26)", "a pointer left out: from what it came to");
  assert.equal(expression("orr", [0x1, 0x2], { value: 0x201, rshift: ["lsl", 8] }), "0x1 | (0x2 << 8)");
  assert.equal(expression("and", [0x6b448cb], { value: 0x480, rshift: ["lsr", 4] }), "masked (0x6b448cb, a constant)", "a mask the code made before, not known");
  assert.equal(expression("and", [0x6b448cb], { value: 0x480, rshift: ["lsr", 4], imm: 0xfc0 }), "0xfc0 & (0x6b448cb >> 4)", "and what it held");
  assert.equal(expression("sub", [0x30], { value: 0xd0, imm: 0x100 }), "0x100 − 0x30", "a constant first");
  assert.equal(expression("sub", [0x130], { value: 0x30, imm: 0x100 }), "0x130 − 0x100");
  assert.equal(expression("weird", [1, 2]), "weird (0x1, 0x2)");
  assert.equal(expression("orr", [0x3ed8, 0x3ed8f207], { value: 0xffc53ed8, rshift: ["lsl", 16] }), "combined (0x3ed8, 0x3ed8f207)", "arithmetic that does not come to it is not said");
  assert.equal(expression("add", [0x10, 0x20], { value: 0x30 }), "0x10 + 0x20");
  assert.equal(expression("sub", [0x10, 0x30], { value: 0x20 }), "0x30 − 0x10", "the way round it comes to");
  assert.equal(expression("neg", [0x63a800], { value: 0xff9c5800 }), "−0x63a800");
  assert.equal(expression("neg", [0xffd23c41], { value: 0x2dc3bf }), "−(−0x2dc3bf)", "a negative number's, as one");
  assert.equal(expression("mla", [3, 4, 100], { value: 112 }), "0x64 + 0x3 × 0x4");
  assert.equal(expression("mla", [100, 3, 4], { value: 112 }), "0x64 + 0x3 × 0x4");
  assert.equal(expression("mul", [6], { value: 36 }), "0x6 × 0x6");
  assert.equal(expression("mul", [6], { value: 60, imm: 10 }), "0x6 × 0xa");
  assert.equal(expression("mul", [6, 7], { value: 42 }), "0x6 × 0x7");
  assert.equal(expression("smull", [0x10000, 0xffffffff], { value: 0xffff0000 }), "low half of 0x10000 × −0x1", "signed, said signed");
  assert.equal(expression("smull", [0x10000, 0xffffffff], { value: 0xffffffff }), "high half of 0x10000 × −0x1");
  assert.equal(expression("smull", [0x64f4bb], { value: 0x682bf2c5 }), "half of 0x64f4bb × a constant");
  assert.equal(expression("smull", [0x64f4bb], { value: 0x682bf2c5, imm: 3 }), "multiplied (0x64f4bb)", "a constant it was not: not said to be");
  assert.equal(expression("rsb", [0x5], { value: 0xfffffffb, imm: 0 }), "0 − 0x5");
  assert.equal(expression("pkh", [0x03fe7160, 0x01adae89], { value: 0xae897160, rshift: ["lsl", 16] }), "bottom of 0x3fe7160, top of (0x1adae89 << 16)");
  // Readings the review found said wrong.
  assert.equal(expression("rsb", [0x10, 0x100], { value: 0xf0 }), "0x100 − 0x10", "reversed, said the right way round");
  assert.equal(expression("rsb", [0x10, 0x100], { value: 0x3f0, rshift: ["lsl", 2] }), "(0x100 << 2) − 0x10");
  assert.equal(expression("sub", [0x10], { value: 0xfc1, rshift: ["lsl", 2], imm: 0x1000 }), "subtracted (0x10)", "neither way round comes to it");
  assert.equal(expression("sub", [0x10], { value: 0x1000 - 0x40, rshift: ["lsl", 2], imm: 0x1000 }), "0x1000 − (0x10 << 2)");
  assert.equal(expression("sbc", [5, 3], { value: 1 }), "0x5 − 0x3 − borrow");
  assert.equal(expression("sbc", [5, 3], { value: 2 }), "0x5 − 0x3");
  assert.equal(expression("adc", [5, 3], { value: 9 }), "0x5 + 0x3 + carry");
  assert.equal(expression("lsl", [1, 0x120], { value: 0 }), "0x1 << 32", "by its bottom byte: 32, all out");
  assert.equal(expression("lsl", [1, 0x103], { value: 8 }), "0x1 << 3");
  assert.equal(expression("asr", [0x80000000], { value: 0xf8000000, imm: 4 }), "−0x80000000 >> 4", "signed, said signed");
  assert.equal(expression("sdiv", [0xfffffff6, 2], { value: 0xfffffffb }), "−0xa ÷ 0x2");
  // A register twice: x + (x << 1) is x × 3; (x << 3) − x, x × 7.
  assert.equal(expression("add", [0x178], { value: 0x468, rshift: ["lsl", 1] }), "0x178 + (0x178 << 1)");
  assert.equal(expression("rsb", [0x4bd], { value: 0x212b, rshift: ["lsl", 3] }), "(0x4bd << 3) − 0x4bd");
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
  const link = readTree({ tag: 4, step: { what: "register", register: "lr", value: 5, t: 1_000_000 }, operands: [] }, null);
  assert.equal(onwards(link).path, "board/derive/r14", "lr, gone on with as r14");
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
  assert.equal(addressExpression(0x2406ad84 + (0x60 << 2), [0x60, 0x2406ad84], 2), "0x2406ad84 + (0x60 << 2)", "told apart by which way round comes to it");
  assert.equal(addressExpression(0x2400_0105, [0x2400_0100, 5], 0), "0x24000100 + 0x5");
  assert.equal(addressExpression(0x903ce0a2, [0x8c0], 0), "0x903cd7e2 + 0x8c0", "the table's own left out: taken from the address");
  assert.equal(addressExpression(0x2406ad84 + (0x60 << 2), [0x60], 2), "0x2406ad84 + (0x60 << 2)");
  assert.ok(isAddress({ what: "merge", op: "ldr" }));
  assert.ok(!isAddress({ what: "merge", op: "add" }));
});

test("a count stepped since long before is gone back to where it began in one jump, and counted", async () => {
  const nodes = new Map();
  const stamp = 29_274_000;
  const tree = tic(0x2cf, stamp);
  // Stepped by the same code for 29 s: since 0, set by other code.
  tree.operands[0].step.written.run = 29_000_000;
  const root = readTree(tree, { time: stamp * 1e6, from: stamp * 1e6 - WINDOW }, nodes);
  const held = root.operands[0];
  const ask = onwards(held);
  assert.ok(ask.jump);
  assert.equal(ask.time, (stamp - 28_000 - 29_000_000) * 1e6, "the window of the run's first step");
  const asked = [];
  const query = async (options, path) => {
    asked.push(options.time);
    if (asked.length === 1) {
      // The first step: 0 to 1, of what other code had set.
      const first = tic(1, 274_000);
      first.operands[0].step.written = { stamp: 200_000, run: 0, by: { pc: 0x0800_1000 } };
      return { tree: first };
    }
    // The setting: a constant stored.
    return { tree: { tag: 2, step: { what: "store", addr: LEVELTIME, size: 4, value: 0, pc: 0x0800_1000 }, operands: [] } };
  };
  await goOn(held, { query, nodes });
  assert.equal(asked.length, 2, "the run's first step, then what set it");
  assert.equal(root.repeats + 1, 0x2cf, "every step, counted: each was one");
  assert.ok(!root.uncounted);
  const before = root.operands[0];
  assert.equal(before.role, "before");
  assert.equal(before.step.value, 0);
  assert.equal(before.operands[0].step.pc, 0x0800_1000, "set by other code");
});

test("steps that were not all the same are not counted", () => {
  assert.equal(count(0x2cf, 0x2ce, 1, 1, 0, 1), 0x2cf);
  assert.equal(count(100, 90, 1, 3, 0, 1), null, "a step of 10 at the end, 3 at the beginning");
  assert.equal(count(100, 80, 2, 10, 0, 1), 10);
  assert.equal(count(5, 4, 1, 1, null, 1), null);
  // 300 steps of 0x1000000 wrap: a turn is 256 of them.
  const [last, first] = [(300 * 0x1000000 + 5) >>> 0, 5];
  assert.equal(count(last, (last - 0x1000000) >>> 0, 1, 0x1000005, first, 1), null, "a turn short enough to be in doubt, and no time to tell by");
  assert.equal(count(last, (last - 0x1000000) >>> 0, 1, 0x1000005, first, 1, 310), 300, "told by the time taken");
  assert.equal(count(0xff, 0xfe, 1, 1, 0, 1), 0xff, "a count by one never in doubt");
});

test("a value stepped round a loop is said as its steps", () => {
  const merge = (value, operand) => ({ tag: value, step: { what: "merge", op: "add", value }, operands: [operand] });
  const start = { tag: 1, step: { what: "merge", op: "pkh", value: 0x1000 }, operands: [] };
  const tree = readTree(merge(0x1000 + 0x10 + 0x30 * 2, merge(0x1000 + 0x10 + 0x30, merge(0x1000 + 0x10, start))), null);
  const steps = loopSteps(tree);
  assert.equal(steps.links.length, 3);
  assert.equal(steps.start.step.op, "pkh");
  assert.deepEqual(steps.added, [0x30, 0x30, 0x10]);
  assert.equal(stepsText(0x1000, steps.added), "0x1000 + 0x10 + 0x30 × 2");
  assert.equal(loopSteps(tree.operands[0]), null, "two are said as they are");
  assert.equal(stepsText(0x10, [-1]), "0x10 − 0x1");
});

test("a value stepped by a value followed is said as its steps too", () => {
  const step = { tag: 50, step: { what: "merge", op: "orr", value: 0x30 }, operands: [] };
  const again = { tag: 50, same: true, step: step.step, operands: [] };
  const add = (value, before, by) => ({ tag: value, step: { what: "merge", op: "add", value }, operands: [before, by] });
  const start = { tag: 1, step: { what: "merge", op: "pkh", value: 0x1000 }, operands: [] };
  const tree = readTree(add(0x1090, again, add(0x1060, add(0x1030, start, step), again)), null);
  const steps = loopSteps(tree);
  assert.equal(steps.links.length, 3);
  assert.equal(steps.start.step.op, "pkh");
  assert.deepEqual(steps.added, [0x30, 0x30, 0x30]);
  assert.deepEqual(steps.by.map((n) => n.tag), [50], "what was added, once");
  assert.equal(stepsText(0x1000, steps.added), "0x1000 + 0x30 × 3");
});

test("a count's step says the value before it, not what it began from", () => {
  const node = readTree({ tag: 3, step: { what: "merge", op: "add", value: 0xd1, imm: 1 }, operands: [{ tag: 4, role: "before", step: { what: "held", value: 0 }, operands: [] }] }, null);
  assert.deepEqual(operandValues(node), [0xd0]);
  const moved = readTree({ tag: 3, step: { what: "merge", op: "add", value: 0x364a53a }, operands: [{ tag: 4, role: "before", step: { what: "held", value: 0xff200000 }, operands: [] }, { tag: 5, step: { what: "store", value: 0xfffe0b24 }, operands: [] }] }, null);
  assert.deepEqual(operandValues(moved), [(0x364a53a - 0xfffe0b24) >>> 0, 0xfffe0b24]);
  const masked = readTree({ tag: 3, step: { what: "merge", op: "and", value: 1 }, operands: [{ tag: 4, role: "before", step: { what: "held", value: 0 }, operands: [] }] }, null);
  assert.deepEqual(operandValues(masked), [null], "not worked back: said in words");
  const turned = readTree({ tag: 3, step: { what: "merge", op: "add", value: 0x83000000, rshift: ["lsl", 16] }, operands: [{ tag: 4, role: "before", step: { what: "held", value: 0x40000000 }, operands: [] }, { tag: 5, step: { what: "store", value: 0x100 }, operands: [] }] }, null);
  assert.deepEqual(operandValues(turned), [0x82000000, 0x100], "the other shifted");
  assert.equal(expression("add", operandValues(turned), turned.step), "0x82000000 + (0x100 << 16)");
});

test("what set a count in the window of its first step is what it began from", async () => {
  const nodes = new Map();
  const stamp = 29_274_000;
  const tree = tic(0x2cf, stamp);
  tree.operands[0].step.written.run = 29_000_000;
  const root = readTree(tree, { time: stamp * 1e6, from: stamp * 1e6 - WINDOW }, nodes);
  const query = async () => {
    // The first step, of a store by other code in the same window.
    const first = tic(1, 274_000);
    first.operands[0] = { tag: 2, step: { what: "store", addr: LEVELTIME, size: 4, value: 0, pc: 0x0800_1000 }, operands: [] };
    return { tree: first };
  };
  await goOn(root.operands[0], { query, nodes });
  assert.equal(root.repeats + 1, 0x2cf);
  assert.equal(root.operands[0].role, "before");
  assert.equal(root.operands[0].step.pc, 0x0800_1000, "began from what other code stored");
});

test("what other code set a count to, from the count's own value, is where it began", async () => {
  const nodes = new Map();
  // S, at P, x = x + 1: 10 of 9; a window back, 9 of what other code at
  // 0x200 made of x's own value (x & 0xff); before that, the count again.
  const root = readTree(tic(10, 3_000), { time: 3e9, from: 3e9 - WINDOW }, nodes);
  const windows = [
    () => {
      const tree = tic(9, 2_000);
      tree.operands[0] = { tag: 20, step: { what: "store", addr: LEVELTIME, size: 4, value: 8, pc: 0x200 }, operands: [{ tag: 21, step: { what: "merge", op: "and", value: 8, imm: 0xff }, operands: [{ tag: 22, step: { what: "held", addr: LEVELTIME, size: 4, value: 0x108, ram: true, written: { stamp: 1_000, by: { pc: P } } }, operands: [] }] }] };
      return tree;
    },
    () => tic(0x108, 1_000),
  ];
  let n = 0;
  const query = async () => ({ tree: windows[n++]() });
  await goOn(root.operands[0], { query, nodes });
  assert.equal(root.repeats, 1, "this and the one before");
  const before = root.operands[0];
  assert.equal(before.role, "before");
  assert.equal(before.step.pc, 0x200, "began from what other code set it to");
  // From there on: how the other code made it, not the count again.
  assert.notEqual(root.repeats + 1, 3);
});

test("a step's value before is worked back only where one reading comes to it", () => {
  const before = (value) => ({ tag: 4, role: "before", step: { what: "held", value }, operands: [] });
  const other = (value) => ({ tag: 5, step: { what: "store", value }, operands: [] });
  // A toggle: x = 1 − x.
  const toggle = readTree({ tag: 3, step: { what: "merge", op: "sub", value: 0 }, operands: [other(1), before(1)] }, null);
  assert.deepEqual(operandValues(toggle), [1, 1]);
  const flipped = readTree({ tag: 3, step: { what: "merge", op: "sub", value: 0xf0 }, operands: [other(0x100), before(0x10)] }, null);
  assert.deepEqual(operandValues(flipped), [0x100, null], "x − c or c − x: both come to it, so neither is said");
});

test("a sum written out is not a value stepped round a loop", () => {
  const add = (value, pc, operand) => ({ tag: value, step: { what: "merge", op: "add", value, pc }, operands: [operand] });
  const start = { tag: 1, step: { what: "held", value: 0x1000 }, operands: [] };
  assert.equal(loopSteps(readTree(add(0x1006, 3, add(0x1003, 2, add(0x1001, 1, start))), null)), null, "three instructions, once each");
  assert.ok(loopSteps(readTree(add(0x1006, 1, add(0x1003, 2, add(0x1001, 1, start))), null)), "one of them again");
});
