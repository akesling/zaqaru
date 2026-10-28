// Checks `debuginfo.js` under Node: node --test web/debuginfo.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { Types, changes, decode, guess, hexBytes, ringbuf, scheduling, score, stride } from "./debuginfo.js";

// Types as `board/typed` gives them: a u8, u16, u32, a pointer to u32, an
// Option<u32> (tag at 0, payload at 4), a struct of them, an array.
const types = new Types().add({
  1: { kind: "base", name: "u8", size: 1, enc: "u" },
  2: { kind: "base", name: "u16", size: 2, enc: "u" },
  3: { kind: "base", name: "u32", size: 4, enc: "u" },
  4: { kind: "pointer", name: "&u32", target: 3 },
  5: { kind: "struct", name: "Option<u32>", size: 8, union: false, fields: [], variants: { discriminant: [0, 3], list: [["0", "None", 0, 6], ["1", "Some", 0, 7]] } },
  6: { kind: "struct", name: "None", size: 8, union: false, fields: [], variants: null },
  7: { kind: "struct", name: "Some", size: 8, union: false, fields: [["__0", 4, 3]], variants: null },
  8: { kind: "struct", name: "Thing", size: 20, union: false, fields: [["count", 0, 3], ["next", 4, 4], ["maybe", 8, 5], ["name", 16, 9]], variants: null },
  9: { kind: "array", name: "[u8; 4]", element: 1, count: 4, stride: 1 },
  10: { kind: "alias", name: "", target: 3 },
  11: { kind: "base", name: "f32", size: 4, enc: "f" },
});

function memory(base, bytes) {
  return (address) => (address >= base && address < base + bytes.length ? bytes[address - base] : null);
}

const le = (n) => [n & 0xff, (n >>> 8) & 0xff, (n >>> 16) & 0xff, n >>> 24];

test("a struct's fields are decoded, a Rust enum by its discriminant, bytes as text", () => {
  const bytes = [...le(42), ...le(0x24000100), ...le(1), ...le(7), ...[0x61, 0x62, 0x63, 0]];
  const node = decode(types, 8, 0x2400_0000, memory(0x2400_0000, bytes), "THING", (a) => (a === 0x24000100 ? "OTHER" : null));
  const [count, next, maybe, name] = node.children;
  assert.equal(count.value, "42 (0x2a)");
  assert.equal(next.target, 0x24000100);
  assert.equal(next.value, "0x24000100 → OTHER");
  assert.equal(maybe.variant, "Some");
  assert.equal(maybe.value, "Some(7)");
  assert.match(name.value, /^b"abc"/);
  assert.equal(node.unknown, false);
});

test("bytes nobody knows make a value unknown rather than wrong", () => {
  const node = decode(types, 5, 0, hexBytes("01000000--------"));
  assert.equal(node.variant, "Some");
  assert.equal(node.children[0].value, "?");
  assert.equal(node.unknown, true);
  assert.equal(decode(types, 5, 0, hexBytes("--------00000000")).value, "?");
});

test("an alias is its target, and a float is a float", () => {
  assert.equal(decode(types, 10, 0, memory(0, le(5))).value, "5");
  assert.equal(decode(types, 11, 0, memory(0, le(0x3fc00000))).value, "1.5");
});

test("what changed between two decodings is each leaf that did, and a variant as a whole", () => {
  const at = (count, tag, payload) => decode(types, 8, 0, memory(0, [...le(count), ...le(0), ...le(tag), ...le(payload), 0, 0, 0, 0]), "THING");
  const diff = changes(at(1, 1, 5), at(2, 0, 5));
  assert.deepEqual(diff.map((d) => [d.path.join("."), d.before, d.after]), [["THING.count", "1", "2"], ["THING.maybe", "Some(5)", "None"]]);
  assert.deepEqual(changes(at(1, 1, 5), at(1, 1, 6)).map((d) => d.path.join(".")), ["THING.maybe.__0"]);
});

test("a Hubris ring buffer reads oldest first, from the slot after the last written", () => {
  const ring = new Types().add({
    1: { kind: "base", name: "u16", size: 2, enc: "u" },
    2: { kind: "base", name: "u32", size: 4, enc: "u" },
    3: { kind: "struct", name: "RingbufEntry<u32>", size: 12, union: false, fields: [["line", 0, 1], ["generation", 2, 1], ["count", 4, 1], ["payload", 8, 2]], variants: null },
    4: { kind: "array", name: "", element: 3, count: 3, stride: 12 },
    5: { kind: "struct", name: "Option<usize>", size: 8, union: false, fields: [], variants: { discriminant: [0, 2], list: [["0", "None", 0, 6], ["1", "Some", 0, 7]] } },
    6: { kind: "struct", name: "None", size: 8, union: false, fields: [], variants: null },
    7: { kind: "struct", name: "Some", size: 8, union: false, fields: [["__0", 4, 2]], variants: null },
    8: { kind: "struct", name: "ringbuf::Ringbuf<u32, u16, 3>", size: 44, union: false, fields: [["last", 0, 5], ["buffer", 8, 4]], variants: null },
  });
  const entry = (line, count, payload) => [line, 0, 0, 0, count, 0, 0, 0, ...le(payload)];
  // Slot 0 written last; slot 2 never.
  const bytes = [...le(1), ...le(0), ...entry(30, 1, 300), ...entry(10, 2, 100), ...entry(0, 0, 0)];
  const found = ringbuf(decode(ring, 8, 0, memory(0, bytes), "RINGBUF"));
  assert.equal(found.last, 0);
  assert.deepEqual(found.log.map((e) => [e.line, e.count, e.payload]), [[10, 2, "100 (0x64)"], [30, 1, "300 (0x12c)"]]);
});

test("a word is guessed at from what it could be", () => {
  const where = (a) => (a >= 0x0800_0000 && a < 0x0810_0000 ? { code: "main" } : a >= 0x2400_0000 && a < 0x2408_0000 ? { data: "BUF" } : null);
  const kinds = [0, 0xbaddcafe, 0x0800_1235, 0x2400_0010, 0x6c6c6568, 7, 0xffffffff, 0x3f800000, 0x9abc_def0].map((w) => guess(w, where).kind);
  assert.deepEqual(kinds, ["zero", "paint", "code", "pointer", "text", "int", "int", "float", "data"]);
});

test("the guesser is scored against what the DWARF says", () => {
  const bytes = [...le(0x3f800000), ...le(0x24000100), ...le(1), ...le(7), 0x61, 0x62, 0x63, 0];
  const tree = decode(types, 8, 0, memory(0, bytes), "THING");
  const where = (a) => (a >= 0x2400_0000 ? { data: "X" } : null);
  const { total, right, misses } = score(types, [tree], memory(0, bytes), where);
  // count (a u32 holding what looks like 1.0), next, and maybe's payload:
  // its tag is no field of any variant.
  assert.equal(total, 3);
  assert.equal(right, 2);
  assert.equal(misses[0].path.join("."), "THING.count");
});

test("writes at a fixed stride are an array's elements", () => {
  assert.deepEqual(stride([0x100, 0x110, 0x130, 0x140]), { stride: 16, elements: 5 });
  assert.equal(stride([0x100, 0x104]), null);
});

test("a task's state says whom it waits on", () => {
  const state = { variant: "Healthy", value: "Healthy(InReply(…))", children: [{ variant: "InReply", value: "InReply(TaskId(0x8003))", children: [{ kind: "struct", children: [{ kind: "base", size: 2, value: "32771 (0x8003)", children: null }] }] }] };
  assert.deepEqual(scheduling(state), { text: "waiting for a reply", on: 3 });
  assert.equal(scheduling({ variant: "Healthy", children: [{ variant: "Runnable", value: "Runnable", children: [] }] }).text, "runnable");
});
