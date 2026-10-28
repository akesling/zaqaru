// What the board's memory holds, read with what the compiler said about it
// (the archive's DWARF, as the store serves it: `board/typed`,
// `board/stack`, `board/types`) and, where it said nothing, guessed at.
//
// A value is decoded from its type into a tree a person can read: a
// struct's fields, the variant a Rust enum holds and its payload, an
// array's elements, where a pointer points. Bytes come from `byte(address)`,
// which says `null` for a byte nobody knows (a variable optimized out, a
// register unwinding could not recover), and a value with unknown bytes
// says so rather than inventing them.
//
// The guesser looks at a word alone: zero, the kernel's stack paint, an
// address in code (a function's, with the Thumb bit) or in RAM, text, a
// small number, a float. It is scored against the DWARF wherever the DWARF
// knows the answer, so how far to trust it where it doesn't is a number,
// not a hope.

/// The kernel's paint on a task's stack, before the task reaches it.
export const STACK_PAINT = 0xbaddcafe;

/// One owner's types, merged as answers bring them, by number.
export class Types {
  constructor() {
    this.all = new Map();
  }

  add(json) {
    for (const [id, type] of Object.entries(json ?? {})) this.all.set(Number(id), type);
    return this;
  }

  get(id) {
    return this.all.get(id) ?? null;
  }

  /// The type `id` is another name for, or itself.
  resolve(id) {
    for (let n = 0; n < 32; n++) {
      const t = this.get(id);
      if (t?.kind !== "alias") return id;
      id = t.target;
    }
    return id;
  }

  name(id) {
    const t = this.get(id);
    if (!t) return "?";
    if (t.name) return t.name;
    if (t.kind === "alias") return this.name(t.target);
    if (t.kind === "pointer") return `*${t.target === null ? "?" : this.name(t.target)}`;
    if (t.kind === "array") return `[${this.name(t.element)}; ${t.count}]`;
    return "?";
  }

  size(id) {
    const t = this.get(this.resolve(id));
    if (!t) return 0;
    if (t.kind === "pointer") return 4;
    if (t.kind === "array") return t.count * t.stride;
    return t.size ?? 0;
  }
}

/// Little-endian, `size` bytes at `at`: a BigInt, or null if any is unknown.
export function bigAt(byte, at, size) {
  let value = 0n;
  for (let i = size - 1; i >= 0; i--) {
    const b = byte(at + i);
    if (b === null || b === undefined) return null;
    value = (value << 8n) | BigInt(b);
  }
  return value;
}

function signed(value, size) {
  const bits = BigInt(8 * size);
  return value >= 1n << (bits - 1n) ? value - (1n << bits) : value;
}

function float(value, size) {
  const view = new DataView(new ArrayBuffer(8));
  if (size === 4) {
    view.setUint32(0, Number(value));
    return view.getFloat32(0);
  }
  view.setBigUint64(0, value);
  return view.getFloat64(0);
}

const hex = (n) => `0x${n.toString(16)}`;

/// The most nodes one decoding makes, and the most elements of an array
/// it shows: a large buffer is not a thousand rows.
const NODES = 4000;
export const ELEMENTS = 64;

/// A value of type `id` at `address`, decoded: `{ name, type, typeName,
/// address, size, kind, value, children, target, variant, unknown, more }`.
/// `value` is a one-line rendering; `children` the parts, for what has
/// them; `target` a pointer's address; `variant` the Rust enum's variant;
/// `unknown` whether its bytes are not all known; `more` how many
/// elements of an array are not shown.
/// `describe(address)` names what a pointer points at, if it can.
export function decode(types, id, address, byte, name = "", describe = () => null, budget = { nodes: NODES }) {
  budget.nodes--;
  const r = types.resolve(id);
  const t = types.get(r);
  const size = types.size(id);
  const node = { name, type: id, typeName: types.name(id), address, size, kind: t?.kind ?? "unknown", value: "", children: null, target: null, variant: null, unknown: false, more: 0 };
  if (!t) {
    node.value = "?";
    return node;
  }
  const all = bigAt(byte, address, Math.min(size, 8));
  if (size > 0 && size <= 8 && all === null) node.unknown = true;
  switch (t.kind) {
    case "base": {
      if (all === null) {
        node.value = "?";
        break;
      }
      if (t.enc === "bool") node.value = all === 0n ? "false" : all === 1n ? "true" : `invalid bool ${hex(all)}`;
      else if (t.enc === "f") node.value = String(float(all, t.size));
      else if (t.enc === "char") node.value = all <= 0x10ffffn ? `'${String.fromCodePoint(Number(all))}'` : `invalid char ${hex(all)}`;
      else if (t.enc === "i") node.value = String(signed(all, t.size));
      else node.value = t.size > 1 && all > 9n ? `${all} (${hex(all)})` : String(all);
      break;
    }
    case "pointer": {
      if (all === null) {
        node.value = "?";
        break;
      }
      node.target = Number(all);
      const what = node.target === 0 ? "null" : describe(node.target);
      node.value = `${hex(all)}${what ? ` → ${what}` : ""}`;
      break;
    }
    case "enum": {
      if (all === null) {
        node.value = "?";
        break;
      }
      const v = t.signed ? signed(all, t.size) : all;
      const named = t.values.find(([, value]) => BigInt(value) === v);
      node.value = named ? named[0] : `? (${v})`;
      node.variant = named?.[0] ?? null;
      break;
    }
    case "array": {
      const shown = Math.min(t.count, ELEMENTS, Math.max(0, budget.nodes));
      node.more = t.count - shown;
      const element = types.get(types.resolve(t.element));
      // Bytes read as bytes: hex, and as text when they are.
      if (element?.kind === "base" && element.size === 1 && element.enc !== "bool") {
        const bytes = [];
        for (let i = 0; i < Math.min(t.count, 256); i++) bytes.push(byte(address + i));
        node.value = bytesText(bytes, t.count);
        if (bytes.some((b) => b === null)) node.unknown = true;
      }
      node.children = [];
      for (let i = 0; i < shown; i++) node.children.push(decode(types, t.element, address + i * t.stride, byte, `[${i}]`, describe, budget));
      if (!node.value) node.value = summary(node);
      break;
    }
    case "struct": {
      if (t.variants) {
        const which = variantOf(types, t, address, byte);
        if (which === undefined) {
          node.value = "?";
          node.unknown = true;
          break;
        }
        if (which === null) {
          node.value = "(no variant matches)";
          break;
        }
        node.variant = which[1];
        const payload = decode(types, which[3], address + which[2], byte, which[1], describe, budget);
        node.children = payload.children ?? [];
        node.unknown ||= payload.unknown;
        node.value = node.children.length ? `${which[1]}(${node.children.map((c) => c.value).join(", ")})` : which[1];
        node.value = clip(node.value);
        break;
      }
      node.children = budget.nodes > 0 ? t.fields.map(([field, offset, type]) => decode(types, type, address + offset, byte, field, describe, budget)) : [];
      node.unknown = node.children.some((c) => c.unknown);
      node.value = summary(node);
      break;
    }
    default: {
      const bytes = [];
      for (let i = 0; i < Math.min(size, 32); i++) bytes.push(byte(address + i));
      node.value = size ? bytesText(bytes, size) : t.name || "()";
    }
  }
  return node;
}

/// Which variant a Rust enum's bytes hold: `[value, name, offset, type]`;
/// `null` when none matches, `undefined` when the discriminant's bytes are
/// not known.
export function variantOf(types, t, address, byte) {
  const { discriminant, list } = t.variants;
  if (!discriminant) return list[0] ?? null;
  const [offset, type] = discriminant;
  const value = bigAt(byte, address + offset, types.size(type));
  if (value === null) return undefined;
  return list.find(([v]) => v !== null && BigInt(v) === value) ?? list.find(([v]) => v === null) ?? null;
}

function clip(text, most = 96) {
  return text.length > most ? `${text.slice(0, most - 1)}…` : text;
}

/// A struct's or array's value on one line: a newtype is its one field's
/// value; a tuple's fields are unnamed.
function summary(node) {
  const parts = node.children ?? [];
  if (parts.length === 1 && /^__0$/.test(parts[0].name)) return parts[0].value;
  const tuple = parts.length && parts.every((c) => /^__\d+$/.test(c.name));
  const inner = parts.map((c) => (tuple || c.name.startsWith("[") ? c.value : `${c.name}: ${c.value}`)).join(", ");
  if (node.kind === "array") return clip(`[${inner}${node.more ? `, …${node.more} more` : ""}]`);
  return clip(tuple ? `(${inner})` : `{ ${inner} }`);
}

/// Bytes as hex, and as a string too when they are mostly text.
function bytesText(bytes, total) {
  const known = bytes.filter((b) => b !== null);
  const hexes = bytes.slice(0, 16).map((b) => (b === null ? "--" : b.toString(16).padStart(2, "0"))).join(" ");
  const more = total > 16 ? ` …${total - 16} more` : "";
  const end = known.indexOf(0);
  const text = known.slice(0, end < 0 ? known.length : end);
  if (text.length >= 3 && text.every((b) => b >= 0x20 && b < 0x7f)) return clip(`b"${String.fromCharCode(...text)}" · ${hexes}${more}`);
  return `${hexes}${more}`;
}

/// The nodes of a tree, depth first, with each one's path from the root.
export function* walk(node, path = []) {
  const here = [...path, node.name];
  yield [node, here];
  for (const child of node.children ?? []) yield* walk(child, here);
}

/// What changed between two decodings of the same thing: each leaf whose
/// value differs, and each enum whose variant does (its payload's changes
/// then said by its variant alone), by path; at most `most`.
export function changes(before, after, most = 200) {
  const out = [];
  const visit = (a, b, path) => {
    if (out.length >= most) return;
    const here = [...path, b.name];
    if (a.variant !== b.variant && (a.variant !== null || b.variant !== null)) return out.push({ path: here, before: a.value, after: b.value, node: b });
    const ac = a.children ?? [];
    const bc = b.children ?? [];
    if (!bc.length || ac.length !== bc.length) {
      if (a.value !== b.value) out.push({ path: here, before: a.value, after: b.value, node: b });
      return;
    }
    for (let i = 0; i < bc.length; i++) visit(ac[i], bc[i], here);
  };
  visit(before, after, []);
  return out;
}

/// A Hubris ring buffer (`ringbuf!`, in `lib/ringbuf`) in a decoded value,
/// found through the wrappers it is kept in (`StaticCell`, `UnsafeCell`,
/// `CountedRingbuf`): its entries oldest first, each its payload, how many
/// times in a row it was recorded, the source line that recorded it and
/// its generation. `null` when the value holds none.
export function ringbuf(node) {
  for (const [n] of walk(node)) {
    if (!/^(ringbuf::)?Ringbuf</.test(n.typeName)) continue;
    const field = (name) => n.children?.find((c) => c.name === name);
    const last = field("last");
    const buffer = field("buffer");
    if (!last || !buffer?.children) continue;
    const entries = buffer.children.length + buffer.more;
    // `last`: an Option<usize>, the slot written last.
    const lastAt = last.variant === "Some" ? Number(last.children?.[0]?.value.split(" ")[0]) : null;
    const out = [];
    const shown = buffer.children;
    if (lastAt !== null && Number.isFinite(lastAt)) {
      for (let k = 1; k <= shown.length; k++) {
        const slot = shown[(lastAt + k) % shown.length];
        const get = (name) => slot?.children?.find((c) => c.name === name);
        const count = Number(get("count")?.value.split(" ")[0] ?? 0);
        if (!count) continue;
        out.push({ payload: get("payload")?.value ?? "?", count, line: Number(get("line")?.value.split(" ")[0] ?? 0), generation: Number(get("generation")?.value.split(" ")[0] ?? 0), node: slot });
      }
    }
    return { node: n, entries, last: lastAt, log: out };
  }
  return null;
}

/// A word, guessed at alone: `{ kind, label }`, the kind one of `zero`,
/// `paint`, `code`, `pointer`, `text`, `int`, `float` or `data`. `where`
/// says what an address is: `{ code: name }` in a function, `{ data: name }`
/// in RAM, or null.
export function guess(word, where = () => null) {
  word >>>= 0;
  if (word === 0) return { kind: "zero", label: "0" };
  if (word === STACK_PAINT) return { kind: "paint", label: "stack paint: never reached" };
  const at = where(word);
  if (at?.code !== undefined && word & 1) return { kind: "code", label: `→ ${at.code} (code, Thumb)` };
  if (at?.code !== undefined) return { kind: "pointer", label: `→ ${at.code} (into code: a return address, a constant)` };
  if (at?.data !== undefined) return { kind: "pointer", label: `→ ${at.data}` };
  const bytes = [word & 0xff, (word >>> 8) & 0xff, (word >>> 16) & 0xff, word >>> 24];
  const printable = (b) => b >= 0x20 && b < 0x7f;
  if (bytes.every(printable) || (bytes.filter(printable).length >= 3 && bytes.every((b) => b === 0 || printable(b))))
    return { kind: "text", label: `"${bytes.map((b) => (printable(b) ? String.fromCharCode(b) : "·")).join("")}"` };
  if (word < 0x10000 || word > 0xffff0000) return { kind: "int", label: String(word > 0xffff0000 ? word - 0x1_0000_0000 : word) };
  const exponent = (word >>> 23) & 0xff;
  if (exponent >= 100 && exponent <= 154) {
    const f = float(BigInt(word), 4);
    return { kind: "float", label: String(Math.abs(f) >= 1e-3 && Math.abs(f) < 1e6 ? +f.toPrecision(6) : f) };
  }
  return { kind: "data", label: hex(word) };
}

/// What a guess of a word should have said, where the DWARF says what is
/// there: the kind of the 4-byte, 4-aligned leaf over it; `null` for what
/// is no such leaf (a byte, a part of something larger).
export function truth(types, node) {
  const t = types.get(types.resolve(node.type));
  if (!t || node.size !== 4 || node.address % 4 !== 0 || node.unknown) return null;
  if (t.kind === "pointer") {
    const target = types.get(types.resolve(t.target));
    return target?.kind === "opaque" && /fn/.test(target.name) ? "code" : "pointer";
  }
  if (t.kind === "base") return t.enc === "f" ? "float" : "int";
  if (t.kind === "enum") return "int";
  return null;
}

/// Whether a guess agrees with the truth: zero is anything's, and a
/// pointer into code is still a pointer.
export function agrees(guessed, truthKind) {
  if (guessed === "zero") return true;
  if (truthKind === "pointer") return guessed === "pointer" || guessed === "code";
  if (truthKind === "code") return guessed === "code";
  if (truthKind === "float") return guessed === "float";
  return guessed === "int" || guessed === "text" || guessed === "paint";
}

/// The guesser scored against decoded values: of the words whose kind the
/// DWARF knows, how many it guessed the kind of, with the misses.
export function score(types, trees, byte, where) {
  let total = 0;
  let right = 0;
  const misses = [];
  for (const tree of trees)
    for (const [node, path] of walk(tree)) {
      if (node.children?.length) continue;
      const kind = truth(types, node);
      if (!kind) continue;
      const word = Number(bigAt(byte, node.address, 4));
      const g = guess(word, where);
      total++;
      if (agrees(g.kind, kind)) right++;
      else if (misses.length < 20) misses.push({ path, truth: kind, guess: g, word });
    }
  return { total, right, misses };
}

/// How a run of memory nobody described seems to be laid out, from where
/// one instruction wrote in it: writes at a fixed stride are an array's
/// elements, the stride their size. `addresses` sorted.
export function stride(addresses) {
  if (addresses.length < 3) return null;
  const gcd = (a, b) => (b ? gcd(b, a % b) : a);
  let g = 0;
  for (let i = 1; i < addresses.length; i++) g = gcd(addresses[i] - addresses[i - 1], g);
  return g >= 4 ? { stride: g, elements: (addresses.at(-1) - addresses[0]) / g + 1 } : null;
}

/// Bytes from a `board/stack` variable's hex: `--` for a byte not known.
export function hexBytes(hexText, base = 0) {
  const bytes = [];
  for (let i = 0; i < hexText.length; i += 2) {
    const pair = hexText.slice(i, i + 2);
    bytes.push(pair === "--" ? null : parseInt(pair, 16));
  }
  return (address) => {
    const i = address - base;
    return i >= 0 && i < bytes.length ? bytes[i] : null;
  };
}

/// A Hubris `TaskId`'s task: its index, the low ten bits (the rest are
/// the generation; see `sys/abi`).
export function taskIndex(id) {
  return Number(id) & 0x3ff;
}

/// What a task is doing, from the kernel's `TaskState`, decoded: whether it
/// is waiting, and on whom. `{ text, on }`, `on` the index of the task it
/// waits on, if any.
export function scheduling(state) {
  if (!state) return null;
  if (state.variant === "Faulted") return { text: `faulted: ${state.value}`, on: null, faulted: true };
  const sched = state.children?.[0];
  if (!sched) return { text: state.value, on: null };
  const variant = sched.variant ?? sched.value;
  const id = (node) => {
    for (const [n] of walk(node)) if (n.kind === "base" && n.size === 2) return taskIndex(n.value.split(" ")[0]);
    return null;
  };
  switch (variant) {
    case "InReply":
      return { text: "waiting for a reply", on: id(sched) };
    case "InSend":
      return { text: "waiting to send", on: id(sched) };
    case "InRecv": {
      const from = sched.children?.[0];
      return from?.variant === "Some" ? { text: "waiting for a message from one task", on: id(from) } : { text: "waiting for a message", on: null };
    }
    default:
      return { text: variant ? variant.replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase() : sched.value, on: null };
  }
}
