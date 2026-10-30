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
  // A merge at a load or a store: the address it worked out.
  ldr: [null, "an address worked out"],
  ldrd: [null, "an address worked out"],
  ldm: [null, "an address worked out"],
  pop: [null, "an address worked out"],
  str: [null, "an address worked out"],
  strd: [null, "an address worked out"],
  stm: [null, "an address worked out"],
  push: [null, "an address worked out"],
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

/// A merge as an expression: its operands' values with the operation
/// between them — `0xa38000 + 0x8000` — or, where no symbol says it, in
/// words. `values`, the operands' values in order.
export function expression(op, values) {
  const { symbol, word } = operation(op);
  const shown = values.map((v) => (v === null || v === undefined ? "?" : `0x${(v >>> 0).toString(16)}`));
  if (symbol && shown.length >= 2) return shown.join(` ${symbol} `);
  return `${word}${shown.length ? ` (${shown.join(", ")})` : ""}`;
}

/// The value a node's step holds, as its parent sees it.
export function valueOf(node) {
  return node?.step?.value ?? null;
}

/// The source line a node's code is at.
export function nodeLine(node) {
  return lineOf(node.step ?? {});
}
