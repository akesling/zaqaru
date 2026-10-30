// How a value was made: its history as a tree (`board/derive`), read top
// down as the value was made bottom up — the store that put it where it
// is, the arithmetic it was made by and of what, the lookups that chose
// it — down to where each part came into the window. A part held in RAM
// since before the window goes on, asked, in the window before the write
// that put it there; one in a register as the window began, in the window
// before that, ending where it began — as a story's do (see trail.js).
//
// The same code storing to the same place what it made of what it had
// stored there before — a count, a sum — is folded, in a window and across
// them, into how many times: `leveltime`, a tic at a time.

import { WINDOW, lineOf } from "./trail.js";

/// What an instruction did to the values it made one of, as a symbol to
/// put between them (`+`) or, where none says it, words (`shifted left`).
const OPS = {
  add: ["+", "added"],
  adc: ["+", "added"],
  sub: ["−", "subtracted"],
  sbc: ["−", "subtracted"],
  rsb: ["−", "subtracted"],
  neg: ["−", "negated"],
  mul: ["×", "multiplied"],
  smull: ["×", "multiplied"],
  umull: ["×", "multiplied"],
  mla: ["×+", "multiplied and added"],
  smlal: ["×+", "multiplied and added"],
  umlal: ["×+", "multiplied and added"],
  mls: ["×−", "multiplied and subtracted"],
  sdiv: ["÷", "divided"],
  udiv: ["÷", "divided"],
  lsl: ["<<", "shifted left"],
  lsr: [">>", "shifted right"],
  asr: [">>", "shifted right"],
  ror: [null, "rotated"],
  and: ["&", "masked"],
  bic: ["&~", "masked off"],
  orr: ["|", "combined"],
  orn: ["|~", "combined"],
  eor: ["^", "exclusive-or'd"],
  mvn: [null, "inverted"],
  sxtb: [null, "sign-extended"],
  sxth: [null, "sign-extended"],
  uxtb: [null, "zero-extended"],
  uxth: [null, "zero-extended"],
  ubfx: [null, "bits taken"],
  sbfx: [null, "bits taken"],
  bfi: [null, "bits put in"],
  bfc: [null, "bits cleared"],
  movt: [null, "top half set"],
  usat: [null, "clamped"],
  ssat: [null, "clamped"],
  clz: [null, "leading zeros counted"],
  rev: [null, "bytes reversed"],
  rev16: [null, "bytes reversed"],
  revsh: [null, "bytes reversed"],
  rbit: [null, "bits reversed"],
  sel: [null, "lanes chosen"],
  pkh: [null, "halves packed"],
  mov: [null, "moved"],
  // A merge at a load or a store: the address it read or wrote at.
  ldr: ["+", "address"],
  ldrd: ["+", "address"],
  ldm: ["+", "address"],
  pop: ["+", "address"],
  str: ["+", "address"],
  strd: ["+", "address"],
  stm: ["+", "address"],
  push: ["+", "address"],
  fpu: [null, "by the FPU"],
};

/// What a merge's instruction did: `{ symbol, word }`, the symbol null
/// where none says it.
export function operation(op) {
  const [symbol, word] = OPS[op] ?? [null, op ?? "made"];
  return { symbol, word };
}

let ids = 0;

/// `board/derive`'s tree as the page keeps it: each node `{ id, tag, role,
/// step, copies, repeats, earliest, cut, same, operands, window }`, with
/// `parent` and the window it was asked of (`{ time, from }`) to go on
/// from. `nodes`, a Map, is given every node by its id.
export function readTree(tree, window, nodes, parent = null) {
  if (!tree) return null;
  const node = {
    id: ++ids,
    tag: tree.tag,
    role: tree.role ?? "value",
    step: tree.step,
    copies: tree.copies ?? [],
    repeats: tree.repeats ?? 0,
    earliest: tree.earliest ?? null,
    cut: !!tree.cut,
    same: !!tree.same,
    pointer: !!tree.pointer,
    window,
    parent,
    operands: [],
  };
  node.operands = (tree.operands ?? []).map((o) => readTree(o, window, nodes, node)).filter(Boolean);
  nodes?.set(node.id, node);
  return node;
}

/// What going on from a node asks, if anything: `{ path, time, from,
/// value? }` — a node left out, of its own window; what was held in RAM
/// since before the window, in the window before the write that put it
/// there; a register's, in the window before, ending where this began,
/// which should then hold `value`.
export function onwards(node) {
  const step = node.step;
  if (node.cut) return { path: `board/derive/n${node.tag}`, time: node.window.time, from: node.window.from };
  if (node.operands.length || node.same) return null;
  switch (step?.what) {
    case "held": {
      if (!step.ram || step.written?.by?.pc === undefined) return null;
      // The stamp is the write's microsecond plus one: its end.
      const time = step.written.stamp * 1e6;
      return { path: `board/derive/${step.addr}`, time, from: time - WINDOW };
    }
    case "register": {
      if (!/^r\d+$/.test(step.register ?? "")) return null;
      const time = step.t * 1000;
      return { path: `board/derive/${step.register}`, time, from: time - WINDOW, value: step.value };
    }
    default:
      return null;
  }
}

/// The store a node's value was stored by, nearest above it: the node of
/// the tree it is a part of.
function storeAbove(node) {
  for (let at = node.parent; at; at = at.parent) if (at.step?.what === "store") return at;
  return null;
}

/// Whether `store` is to the place `held` was held in — and, if `writer`,
/// by the code that wrote what it held.
function samePlace(store, held, writer = true) {
  const stores = [store.step, ...store.copies];
  return stores.some((x) => x.what === "store" && held.addr >= x.addr && held.addr < x.addr + Math.max(1, x.size) && (!writer || held.written?.by?.pc === x.pc));
}

/// The store above `node` that `node` — something held since before the
/// window — is that store's own value before it: a count, a sum, made a
/// window at a time.
export function recurrence(node) {
  if (node.step?.what !== "held") return null;
  let store = storeAbove(node);
  // Within a fold, what it began from: the fold's own store, above it.
  for (let at = node; at && at !== store; at = at.parent) if (at.role === "before") store = storeAbove(at);
  if (store?.role === "before") store = storeAbove(store);
  return store && samePlace(store, node.step) ? store : null;
}

/// Puts what going on from `node` found, `found` (a tree read by
/// `readTree`), in its place. Where `node` is a store's own value before
/// (`recurrence`) and `found` is the same store a window back, the two are
/// folded: the store's count goes up by what `found` was of, and `node`
/// becomes what `found` began from, to go on from again. Otherwise `found`
/// is what `node` was made of. Returns the node to go on from next, if a
/// fold left one.
export function splice(node, found, nodes) {
  const store = recurrence(node);
  if (store && found?.step?.what === "store" && samePlace(store, node.step)) {
    store.repeats += 1 + found.repeats;
    store.earliest = found.earliest ?? found.step;
    // What it began from: a fold of its own's, or its own value held.
    const base = find(found, (n) => n.role === "before") ?? find(found, (n) => n !== found && n.step?.what === "held" && samePlace(store, n.step, false));
    if (base) {
      // In the place of what the store began from, said once: the fold's
      // own, where the window it was found in had folded some already.
      let target = node;
      for (let at = node.parent; at && at !== store; at = at.parent) if (at.role === "before") target = at;
      Object.assign(target, { ...base, id: target.id, parent: target.parent, role: "before", closed: undefined });
      for (const o of target.operands) o.parent = target;
      nodes?.set(target.id, target);
      // Its own value held since before that window: to go on from.
      return find(target, (n) => n.step?.what === "held" && samePlace(store, n.step));
    }
  }
  if (found) {
    found.parent = node;
    node.operands = [found];
  }
  node.cut = false;
  return null;
}

function find(node, test) {
  if (test(node)) return node;
  for (const o of node.operands) {
    const hit = find(o, test);
    if (hit) return hit;
  }
  return null;
}

/// Goes on from `node` (see `onwards`): asks, splices what it found, and
/// while that was a fold, goes on again, `most` windows at most, until
/// `wanted()` says not. `query(options, path)` reads from a re-execution;
/// `update()` after each window. Sets `node.error` or `node.lost`.
export async function goOn(node, { query, update = () => {}, wanted = () => true, nodes = null, most = 16 }) {
  for (let n = 0; n < most && node && wanted(); n++) {
    const ask = onwards(node);
    if (!ask) return;
    node.running = true;
    update();
    let answer;
    try {
      answer = await query({ time: ask.time, follow: { from: Math.max(0, ask.from) } }, ask.path);
    } catch (why) {
      node.running = false;
      node.error = String(why);
      return;
    }
    node.running = false;
    // A register asked about: what it held where the window ends should be
    // what it was found holding, or this is not the instant it was.
    if (ask.value !== undefined && answer?.value !== undefined && answer.value !== ask.value) {
      node.lost = { expected: ask.value, found: answer.value };
      return;
    }
    const found = readTree(answer?.tree ?? null, { time: ask.time, from: ask.from }, nodes);
    if (!found) {
      node.ended = true;
      return;
    }
    node = splice(node, found, nodes);
    update();
  }
}

/// Every node, depth first.
export function nodesOf(root) {
  const out = [];
  const visit = (n) => {
    if (!n) return;
    out.push(n);
    n.operands.forEach(visit);
  };
  visit(root);
  return out;
}

/// A derivation as heat for the memory pictures: each place a part of it
/// was stored, held or looked up — `[{ addr, length, weight, name }]`.
export function deriveField(root) {
  const points = [];
  for (const n of nodesOf(root)) {
    for (const step of [n.step, ...n.copies]) {
      if (!step || step.addr === undefined || step.what === "read" || step.what === "merge") continue;
      points.push({ addr: step.addr, length: Math.max(1, step.size ?? 1), weight: 1, name: (step.where ?? "").replace(/^[\w-]+ (\.[\w.]+|stack|code): /, "").replace(/(\[\d+\]|\s\+\s0x[0-9a-f]+).*$/, "") || "somewhere" });
    }
  }
  return points;
}

/// A node's value as the tree shows it: hex, and signed decimal where
/// that is shorter to read.
export function valueText(value) {
  if (value === null || value === undefined) return "?";
  const v = value >>> 0;
  const signed = v | 0;
  if (v < 10) return String(v);
  const hex = `0x${v.toString(16)}`;
  return Math.abs(signed) < 100000 ? `${hex} (${signed})` : hex;
}

const hex = (v) => (v === null || v === undefined ? "?" : `0x${(v >>> 0).toString(16)}`);

/// `v` shifted as an instruction shifts its second operand.
function shiftBy(v, kind, n) {
  v >>>= 0;
  switch (kind) {
    case "lsl":
      return n >= 32 ? 0 : (v << n) >>> 0;
    case "lsr":
      return n >= 32 ? 0 : v >>> n;
    case "asr":
      return (n >= 32 ? (v | 0) >> 31 : (v | 0) >> n) >>> 0;
    default:
      return ((v >>> n) | (v << (32 - n))) >>> 0;
  }
}

/// What an operation makes of two values, where it is one of the few
/// that are a symbol between them: to tell its operands apart.
function apply(op, a, b) {
  const f = { add: a + b, adc: a + b, sub: a - b, rsb: b - a, orr: a | b, eor: a ^ b, and: a & b, bic: a & ~b }[op];
  return f === undefined ? null : f >>> 0;
}

/// A merge as an expression: its operands' values with the operation
/// between them — `0xa38000 + 0x8000` — its second operand shifted where
/// the instruction shifts it (`step.rshift`), the constant it worked with
/// (`step.imm`, `step.bits`): `0x10c492e8 >> 10`, `bits 4–9 of 0x1f3`; and
/// where a pointer was left out (`pointer`), the pointer, from what the
/// rest came to. Or, where no symbol says it, in words. Null with no
/// operand to say.
export function expression(op, values, step = {}, pointer = false) {
  const { symbol, word } = operation(op);
  if (!values.length) return null;
  const shift = ["lsl", "lsr", "asr", "ror"].includes(op);
  const value = step.value;
  const shifted = (v) => (step.rshift ? `${hex(v)} ${{ lsl: "<<", lsr: ">>", asr: ">>", ror: "rotated" }[step.rshift[0]]} ${step.rshift[1]}` : hex(v));
  if (step.rshift && symbol) {
    const [kind, n] = step.rshift;
    if (values.length === 2) {
      // Which was shifted: the way round that comes to the value.
      for (const [a, b] of [values, [...values].reverse()]) if (apply(op, a, shiftBy(b, kind, n)) === value >>> 0) return `${hex(a)} ${symbol} ${shifted(b)}`;
    }
    if (values.length === 1 && pointer && (op === "add" || op === "adc")) return `${hex(value - shiftBy(values[0], kind, n))} ${symbol} ${shifted(values[0])}`;
    // The other a constant the code had made before: not followed.
    if (values.length === 1 && !pointer) return `${shifted(values[0])} ${symbol} a constant`;
  }
  if (values.length === 1 && pointer && symbol === "+" && value !== null && value !== undefined) return `${hex(value - values[0])} + ${hex(values[0])}`;
  if (step.bits && values.length === 1) {
    const [lsb, width] = step.bits;
    return `${op === "bfi" || op === "bfc" ? word : "bits"} ${lsb}–${lsb + width - 1} of ${hex(values[0])}`;
  }
  if (step.imm !== undefined && values.length === 1) {
    const imm = shift || step.imm < 10 ? String(step.imm) : hex(step.imm);
    if (symbol) return `${hex(values[0])} ${symbol} ${imm}`;
    if (shift) return `${hex(values[0])} rotated by ${imm}`;
  }
  if (symbol && values.length >= 2) return values.map(hex).join(` ${symbol} `);
  return `${word} (${values.map(hex).join(", ")})`;
}

/// Whether a merge is an address a load or a store worked out.
export function isAddress(step) {
  return step?.what === "merge" && operation(step.op).word === "address";
}

/// An address worked out of a base and an index, as an expression: `base
/// + index << shift` — the two told apart by which way round comes to
/// `value` — or, of one, or of more, their sum.
export function addressExpression(value, values, shift = 0) {
  const known = values.filter((v) => v !== null && v !== undefined);
  // The table's own address left out: what the index comes to, taken from
  // the address.
  if (known.length === 1 && value !== null && value !== undefined) {
    const index = known[0];
    return `${hex(value - index * 2 ** shift)} + ${hex(index)}${shift ? ` << ${shift}` : ""}`;
  }
  if (known.length === 2 && value !== null && value !== undefined) {
    for (const [base, index] of [known, [...known].reverse()]) {
      if (((base + index * 2 ** shift) >>> 0) === value >>> 0) return `${hex(base)} + ${hex(index)}${shift ? ` << ${shift}` : ""}`;
    }
  }
  return known.length ? known.map(hex).join(" + ") : hex(value ?? 0);
}

/// The value a node's step holds, as its parent sees it.
export function valueOf(node) {
  return node?.step?.value ?? null;
}

/// The source line a node's code is at.
export function nodeLine(node) {
  return lineOf(node.step ?? {});
}
