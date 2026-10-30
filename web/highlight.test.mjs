// Checks `highlight.js` under Node: node --test web/highlight.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { highlight, tokens } from "./highlight.js";

const classes = (text, file) => tokens(text, file).filter(([c]) => c).map(([c, t]) => `${c}:${t}`);

test("Rust: keywords, types, calls, macros, lifetimes, attributes, strings and numbers", () => {
  assert.deepEqual(classes(`#[inline(always)]\npub fn copy<'a>(x: &'a [u8], n: usize) -> Option<u32> { let c = b'\\n'; debug_assert!(n != 0x10_u32); f::<u8>(r#"a"b"#) }`, "src/lib.rs"), [
    "a:#[inline(always)]",
    "k:pub", "k:fn", "f:copy", "l:'a", "l:'a", "t:u8", "t:usize", "t:Option", "t:u32",
    "k:let", "s:b'\\n'", "m:debug_assert!", "n:0x10_u32", "f:f", "t:u8", `s:r#"a"b"#`,
  ]);
});

test("C: the preprocessor, types, calls and constants", () => {
  assert.deepEqual(classes(`#include <stdio.h>\nstatic int8_t f (int x) { return x * FRACUNIT + 1.5f; } // done`, "doomgeneric/r_draw.c"), [
    "a:#include <stdio.h>", "k:static", "t:int8_t", "f:f", "t:int", "k:return", "x:FRACUNIT", "n:1.5f", "c:// done",
  ]);
});

test("a comment over many lines is a comment on each, the rest escaped", () => {
  assert.deepEqual(highlight("/* a\n b */ x < 1", "a.c"), [`<span class="hl-c">/* a</span>`, `<span class="hl-c"> b */</span> x &lt; <span class="hl-n">1</span>`]);
  assert.deepEqual(highlight("plain <text>\n", "notes.txt"), ["plain &lt;text&gt;", ""]);
  assert.deepEqual(classes("x1 = a2", "a.rs"), []);
});
