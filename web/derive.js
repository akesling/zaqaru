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
/// value?, jump? }` — a node left out, of its own window; what was held in
/// RAM since before the window, in the window before the write that put it
/// there; a register's, in the window before, ending where this began,
/// which should then hold `value`. A store's own value before (see
/// `recurrence`), written by the same code for longer than a window — a
/// count, stepped since something else set it — in the window of the
/// first of those writes, `jump`, rather than a window at a time.
export function onwards(node) {
  const step = node.step;
  if (node.cut) return { path: `board/derive/n${node.tag}`, time: node.window.time, from: node.window.from };
  if (node.operands.length || node.same) return null;
  switch (step?.what) {
    case "held": {
      if (!step.ram || step.written?.by?.pc === undefined) return null;
      const { stamp, run = 0 } = step.written;
      // The stamp is the write's microsecond plus one: its end.
      if (run * 1e6 > WINDOW && recurrence(node)) {
        const time = (stamp - run) * 1e6;
        return { path: `board/derive/${step.addr}`, time, from: time - WINDOW, jump: true };
      }
      const time = stamp * 1e6;
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
/// fold left one. `jump`: `found` is the first of the store's run, the
/// steps between not seen — counted, where each was the same step (see
/// `count`), or not.
export function splice(node, found, nodes, jump = false) {
  const store = recurrence(node);
  if (store && found?.step?.what === "store" && samePlace(store, node.step)) {
    const [seen, held] = [store.repeats, node.step.value];
    store.repeats += 1 + found.repeats;
    store.earliest = found.earliest ?? found.step;
    // What it began from: a fold of its own's, or its own value held.
    // Or other code's store there, in the same window: what set it.
    const set = (n) => n !== found && samePlace(store, n.step, false) && (n.step?.what === "held" || (n.step?.what === "store" && n.step.pc !== store.step.pc));
    const base = find(found, (n) => n.role === "before") ?? find(found, set);
    if (base) {
      // In the place of what the store began from, said once: the fold's
      // own, where the window it was found in had folded some already.
      let target = node;
      for (let at = node.parent; at && at !== store; at = at.parent) if (at.role === "before") target = at;
      Object.assign(target, { ...base, id: target.id, parent: target.parent, role: "before", closed: undefined });
      for (const o of target.operands) o.parent = target;
      nodes?.set(target.id, target);
      if (jump) {
        // What it held before the first step: the base, or within a fold's
        // own, what that began from.
        const first = set(base) ? base : find(base, (n) => n.step?.what === "held" && samePlace(store, n.step, false));
        const times = count(store.step.value, held, seen + 1, found.step.value, first?.step?.value, found.repeats + 1);
        if (times === null) store.uncounted = true;
        else store.repeats = times - 1;
      }
      // Its own value held since before that window: to go on from — a
      // fold again, or what other code set it to, where it began.
      return find(target, (n) => n.step?.what === "held" && samePlace(store, n.step, false));
    }
    if (jump) store.uncounted = true;
  }
  if (found) {
    found.parent = node;
    node.operands = [found];
  }
  node.cut = false;
  return null;
}

/// How many times a store was made of what it held before, from `first`
/// to `last`, where each time was the same step: the steps seen at the
/// end, `seen` of them from `before` to `last`, and at the beginning,
/// `began` from `first` to `after`, each the same, and the whole a whole
/// number of them. Null where they were not — a position moved by
/// different amounts — and the steps between are not known.
export function count(last, before, seen, after, first, began) {
  if ([last, before, after, first].some((v) => v === null || v === undefined)) return null;
  const diff = (a, b) => (a - b) | 0;
  const step = diff(last, before) / seen;
  if (!Number.isInteger(step) || step === 0 || diff(after, first) !== step * began) return null;
  const times = diff(last, first) / step;
  return Number.isInteger(times) && times > 0 ? times : null;
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
    node = splice(node, found, nodes, !!ask.jump);
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
  const f = { add: a + b, adc: a + b, sub: a - b, rsb: b - a, orr: a | b, eor: a ^ b, and: a & b, bic: a & ~b, mul: Math.imul(a, b) }[op];
  return f === undefined ? null : f >>> 0;
}

/// A merge as an expression: its operands' values with the operation
/// between them — `0xa38000 + 0x8000` — its second operand shifted where
/// the instruction shifts it (`step.rshift`), the constant it worked with
/// (`step.imm`, an immediate or a register's the code made before;
/// `step.bits`): `0x10c492e8 >> 10`, `bits 4–9 of 0x1f3`; and
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
    // The other a constant the code had made before (`step.imm`), the way
    // round that comes to the value; or, not known, said to be one.
    if (values.length === 1 && !pointer) {
      const [v, c] = [values[0], step.imm];
      if (c !== undefined && apply(op, v, shiftBy(c, kind, n)) === value >>> 0) return `${hex(v)} ${symbol} ${shifted(c)}`;
      if (c !== undefined && (op === "add" || op === "adc")) return `${hex(c)} + ${shifted(v)}`;
      return `${shifted(v)} ${symbol} ${c === undefined ? "a constant" : hex(c)}`;
    }
  }
  const known = value !== null && value !== undefined;
  if (op === "neg" && values.length === 1) return (values[0] | 0) < 0 ? `−(−${hex(-(values[0] | 0))})` : `−${hex(values[0])}`;
  // A multiply and an add: which two were multiplied, by what it came to.
  if ((op === "mla" || op === "mls") && values.length === 3 && known) {
    for (const [a, b, c] of [values, [values[1], values[2], values[0]], [values[0], values[2], values[1]]]) {
      const product = Math.imul(a, b);
      if ((op === "mla" ? (c + product) >>> 0 : (c - product) >>> 0) === value >>> 0) return `${hex(c)} ${op === "mla" ? "+" : "−"} ${hex(a)} × ${hex(b)}`;
    }
  }
  // A long multiply: the half of the product it came to.
  if ((op === "smull" || op === "umull" || op === "smlal" || op === "umlal") && known && values.length >= 1 && values.length <= 2) {
    const big = (v) => (op[0] === "s" ? BigInt(v | 0) : BigInt(v >>> 0));
    const [a, b] = values.length === 2 ? values : [values[0], step.imm ?? values[0]];
    const product = BigInt.asUintN(64, big(a) * big(b));
    const said = values.length === 2 || step.imm !== undefined ? `${hex(a)} × ${hex(b)}` : `${hex(a)} × ${hex(a)}`;
    if (Number(product & 0xffffffffn) === value >>> 0) return `low half of ${said}`;
    if (Number(product >> 32n) === value >>> 0) return `high half of ${said}`;
    return values.length === 1 && step.imm === undefined ? `half of ${hex(a)} × a constant` : `${word} (${values.map(hex).join(", ")})`;
  }
  // Of one value followed: by itself, or by a constant not known.
  if (op === "mul" && values.length === 1 && known && step.imm === undefined) return Math.imul(values[0], values[0]) >>> 0 === value >>> 0 ? `${hex(values[0])} × ${hex(values[0])}` : `${hex(values[0])} × a constant`;
  if (op === "rsb" && values.length === 1 && step.imm !== undefined && !step.rshift) return `${step.imm < 10 ? step.imm : hex(step.imm)} − ${hex(values[0])}`;
  // Halves packed: the bottom of one and the top of the other, shifted —
  // the way round that comes to the value.
  if (op === "pkh" && values.length === 2 && known) {
    const [kind, n] = step.rshift ?? ["lsl", 0];
    for (const [a, b] of [values, [...values].reverse()]) {
      const s = shiftBy(b, kind, n);
      const said = `${hex(b)}${n ? ` ${kind === "lsl" ? "<<" : ">>"} ${n}` : ""}`;
      if ((((a & 0xffff) | (s & 0xffff0000)) >>> 0) === value >>> 0) return `bottom of ${hex(a)}, top of ${said}`;
      if ((((a & 0xffff0000) | (s & 0xffff)) >>> 0) === value >>> 0) return `top of ${hex(a)}, bottom of ${said}`;
    }
  }
  if (values.length === 1 && pointer && symbol === "+" && value !== null && value !== undefined) return `${hex(value - values[0])} + ${hex(values[0])}`;
  if (step.bits && values.length === 1) {
    const [lsb, width] = step.bits;
    return `${op === "bfi" || op === "bfc" ? word : "bits"} ${lsb}–${lsb + width - 1} of ${hex(values[0])}`;
  }
  if (step.imm !== undefined && values.length === 1) {
    const imm = shift || step.imm < 10 ? String(step.imm) : hex(step.imm);
    // A constant first: `0x100 − x`, where that is what it came to.
    if (symbol && !shift && apply(op, step.imm, values[0]) === value >>> 0 && apply(op, values[0], step.imm) !== value >>> 0) return `${imm} ${symbol} ${hex(values[0])}`;
    if (symbol) return `${hex(values[0])} ${symbol} ${imm}`;
    if (shift) return `${hex(values[0])} rotated by ${imm}`;
  }
  if (symbol && values.length >= 2) {
    // Said only where it is so: arithmetic that does not come to the value
    // — an operand shifted some way not known — is said in words.
    const checks = values.length === 2 && known && apply(op, 0, 0) !== null;
    if (checks && !step.rshift) {
      for (const [a, b] of [values, [...values].reverse()]) if (apply(op, a, b) === value >>> 0) return `${hex(a)} ${symbol} ${hex(b)}`;
    }
    if (!checks) return values.map(hex).join(` ${symbol} `);
  }
  return `${word} (${values.map(hex).join(", ")})`;
}

/// A value stepped a few times in a row — `position += step` round a
/// loop, each step of the one before — as one: the steps, newest first,
/// what the first was taken from (`start`), how much each added, and what
/// the steps added that was followed (`by`, each once). Of a step's
/// operands, the one before is the step — an add of its own — or, at the
/// first, the one that is not what the steps after it added. Null for
/// fewer than `least`.
export function loopSteps(node, least = 3) {
  const adds = (n) => n?.step?.what === "merge" && ["add", "sub", "adc", "sbc"].includes(n.step.op) && n.step.value !== null && n.step.value !== undefined && n.operands.length >= 1 && n.operands.length <= 2 && n.operands.every((o) => o.role === "value");
  const links = [];
  const by = [];
  let at = node;
  let start = null;
  while (adds(at)) {
    const [a, b] = at.operands;
    let before = null;
    if (!b) before = a;
    else if (adds(a) !== adds(b)) before = adds(a) ? a : b;
    else {
      // Neither, or both, a step: the one that is not what was added before.
      const added = new Set(by.map((n) => n.tag));
      if (added.has(a.tag) !== added.has(b.tag)) before = added.has(a.tag) ? b : a;
    }
    if (!before) break;
    links.push(at);
    const other = b && (before === a ? b : a);
    // Each once, said in full where it is somewhere.
    const known = other ? by.findIndex((n) => n.tag === other.tag) : -1;
    if (other && known < 0) by.push(other);
    else if (other && by[known].same && !other.same) by[known] = other;
    start = before;
    at = before;
  }
  if (links.length < least || valueOf(start) === null) return null;
  const values = [...links.map(valueOf), valueOf(start)];
  const added = links.map((_, i) => (values[i] - values[i + 1]) | 0);
  return { links, start, added, by };
}

/// Steps as an expression, the first first: `0xb6ba7160 + 0xfceb + 0x3f3ac
/// × 2`, a run of the same said once with how many.
export function stepsText(start, added) {
  const runs = [];
  for (const a of [...added].reverse()) {
    if (runs.length && runs.at(-1)[0] === a) runs.at(-1)[1]++;
    else runs.push([a, 1]);
  }
  const said = runs.map(([a, n]) => `${a < 0 ? "−" : "+"} ${hex(Math.abs(a))}${n > 1 ? ` × ${n}` : ""}`);
  const shown = said.length > 8 ? [...said.slice(0, 4), `… ${said.length - 6} more …`, ...said.slice(-2)] : said;
  return `${hex(start)} ${shown.join(" ")}`;
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

/// The values a merge was made of, as its expression says them: each
/// operand's, but a store's own value before the repeats (`before`) — said
/// as what it began from — worked back from what the merge came to where
/// the rest say how, or null where they do not.
export function operandValues(node) {
  const step = node.step ?? {};
  const values = node.operands.map(valueOf);
  const i = node.operands.findIndex((o) => o.role === "before");
  if (i < 0) return values;
  const value = step.value;
  const others = values.filter((_, j) => j !== i);
  let was = null;
  if (value !== null && value !== undefined) {
    let other = others.length === 1 ? others[0] : others.length === 0 ? step.imm : undefined;
    // The other the one shifted: the place's own value is the first.
    if (other !== undefined && other !== null && step.rshift) other = shiftBy(other, ...step.rshift);
    if (other !== undefined && other !== null) {
      const v = { add: value - other, adc: value - other, sub: value + other, eor: value ^ other }[step.op];
      if (v !== undefined) was = v >>> 0;
    }
  }
  values[i] = was;
  return values;
}

/// The value a node's step holds, as its parent sees it.
export function valueOf(node) {
  return node?.step?.value ?? null;
}

/// The source line a node's code is at.
export function nodeLine(node) {
  return lineOf(node.step ?? {});
}
