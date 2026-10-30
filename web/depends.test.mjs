// Checks `depends.js` under Node: node --test web/depends.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { alike, depend, dependsField, dependsSummary, family, grouped, pathTo, reachField, timeStrip } from "./depends.js";

const by = { pc: 0x0800_1234, code: "doom: R_DrawColumn" };
const held = (addr, stamp, via = "value") => ({ what: "held", ram: true, addr, byte: addr, size: 1, value: 1, via, where: `doom .uninit: ZONE_AXI[${addr & 0xffff}] (AXI SRAM + 0x1)`, written: { stamp, by } });
const wad = (lump, via = "value") => ({ what: "held", ram: false, addr: 0x9000_0000, size: 1, value: 2, via, where: `the NOR: WAD lump ${lump} + 0x10: light map 1, colour 0x10` });

test("every value a value depended on is followed back, breadth first, to where it came in", async () => {
  // The pixel: from a colour map entry, chosen by a texel held in RAM; the
  // texel from a texture in the WAD, chosen by a column held in RAM; the
  // column from a byte off USART3.
  const answers = {
    [`2400 ${30e9}`]: [wad("COLORMAP"), held(0x2400_9000, 20000, "address")],
    [`24009000 ${20e9}`]: [wad("SW11_1"), held(0x2400_a000, 10000, "address")],
    [`2400a000 ${10e9}`]: [{ what: "read", register: "USART3 RDR", value: 0x77, via: "value" }],
  };
  const asked = [];
  const query = async ({ time }, path) => {
    const at = Number(path.split("/").at(-1)).toString(16);
    asked.push(at);
    return { what: at, steps: [], influences: answers[`${at} ${time}`] ?? [] };
  };
  const graph = { address: 0x2400, nodes: [], origins: [], from: { at: 0x2400, time: 30e9 } };
  await depend(graph, { read: async () => assert.fail(), query });
  assert.deepEqual(asked, ["2400", "24009000", "2400a000"]);
  assert.deepEqual(graph.origins.map((o) => [o.kind, family(o), o.via]), [
    ["rom", "WAD lump COLORMAP", "value"],
    ["rom", "WAD lump SW11_1", "value"],
    ["bus", "USART3 RDR", "value"],
  ]);
  assert.deepEqual(pathTo(graph, 2).map((n) => n.at), [0x2400_a000, 0x2400_9000, 0x2400]);
  assert.deepEqual(grouped(graph).map((g) => g.kind), ["bus", "rom"]);
  assert.match(dependsSummary(graph), /^It depended on 3 values, followed back through 3 writes — from outside: USART3 RDR; from data: WAD lump COLORMAP, WAD lump SW11_1\.$/);
  assert.match(dependsSummary(graph, 1), /from data: WAD lump COLORMAP and 1 more\./);
});

test("the walk stops at its budget and says what it left", async () => {
  let n = 0;
  const query = async () => ({ steps: [], influences: [held(0x2400_0000 + ++n, 1000 - n), held(0x2500_0000 + n, 1000 - n)] });
  const graph = { address: 1, nodes: [], origins: [], from: { at: 1, time: 1e12 } };
  await depend(graph, { read: async () => ({}), query, budget: 3 });
  assert.equal(graph.nodes.length, 3);
  assert.equal(graph.left, 4);
  await depend(graph, { read: async () => ({}), query, budget: 2 });
  assert.equal(graph.nodes.length, 5, "and goes on from there");
});

test("a value in a register is followed into the window before, as small as the first", async () => {
  const asked = [];
  const query = async ({ time, follow: { from } }, path) => {
    asked.push([path.split("/").at(-1), time, time - from]);
    return path.endsWith("r3") ? { value: 5, steps: [], influences: [wad("PLAYPAL")] } : { steps: [], influences: [{ what: "register", register: "r3", value: 5, t: 9_000_000, via: "value" }] };
  };
  const graph = { address: 1, nodes: [], origins: [], from: { at: 1, time: 1e10 } };
  await depend(graph, { read: async () => ({}), query });
  assert.deepEqual(asked, [["1", 1e10, 1e9], ["r3", 9e9, 1e9]]);
  assert.equal(graph.origins[0].kind, "rom");
});

test("a strip of time spreads what was a microsecond before and what was a second before", () => {
  const svg = timeStrip([{ t: 1e9, kind: "step", label: "asked" }, { t: 1e9 - 1e3, kind: "step", label: "a µs" }, { t: 0, kind: "origin", label: "a s" }]);
  const xs = [...svg.matchAll(/<circle cx="([\d.]+)"/g)].map((m) => Number(m[1]));
  assert.equal(xs.length, 3);
  assert.ok(xs[0] > xs[1] && xs[1] > xs[2] && xs[0] - xs[1] > 100, `${xs}`);
  assert.equal(timeStrip([{ t: 1, kind: "step", label: "one" }]), "");
});

test("places are grouped without which element, entry or offset; constants by the code that made them", () => {
  assert.equal(family({ kind: "rom", leaf: { where: "the NOR: the WAD's directory, entry 552" } }), "the WAD's directory");
  assert.equal(family({ kind: "constant", leaf: { code: "doom: G_BuildTiccmd + 0x10", lines: [{ function: "G_BuildTiccmd", file: "g_game.c", line: 332 }] } }), "made in G_BuildTiccmd");
});

test("the same origin found again and again is one, with how many", () => {
  const status = { kind: "bus", via: "address", leaf: { what: "held", where: "USART3 + 0x1c", value: 0, pc: 8 } };
  const other = { ...status, leaf: { ...status.leaf, value: 0x20 } };
  assert.deepEqual(alike([status, status, other, status]).map((a) => a.count), [3, 1]);
});

test("what a value depended on, and where one went, as heat", () => {
  const graph = { nodes: [{ at: 0x2400, what: "doom .uninit: ZONE_AXI[3] (AXI SRAM + 0x3)" }], origins: [{ kind: "rom", leaf: wad("COLORMAP"), node: 0, via: "value" }, { kind: "bus", leaf: { what: "read", register: "USART3 RDR" }, node: 0, via: "value" }] };
  assert.deepEqual(dependsField(graph), [
    { addr: 0x2400, length: 1, weight: 1, name: "ZONE_AXI" },
    { addr: 0x9000_0000, length: 1, weight: 1, name: "WAD lump COLORMAP" },
  ]);
  assert.deepEqual(reachField({ went: [{ addr: 8, length: 4, writes: 8, where: "x .bss: FRAME[2]" }] }), [{ addr: 8, length: 4, weight: 8, name: "FRAME" }]);
});
