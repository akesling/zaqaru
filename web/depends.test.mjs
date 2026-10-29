// Checks `depends.js` under Node: node --test web/depends.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { depend, dependsSummary, family, grouped, pathTo, timeStrip } from "./depends.js";

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
    ["rom", "the NOR: WAD lump COLORMAP", "value"],
    ["rom", "the NOR: WAD lump SW11_1", "address"],
    ["bus", "USART3 RDR", "address"],
  ]);
  assert.deepEqual(pathTo(graph, 2).map((n) => n.at), [0x2400_a000, 0x2400_9000, 0x2400]);
  assert.deepEqual(grouped(graph).map((g) => g.kind), ["bus", "rom"]);
  assert.match(dependsSummary(graph), /^It depended on 3 values \(2 of them chose which value rather than being one\), followed back through 3 writes — from outside: USART3 RDR; from data: the NOR: WAD lump COLORMAP, the NOR: WAD lump SW11_1\.$/);
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

test("a value that sat in a register widens its window once", async () => {
  const windows = [];
  const query = async ({ time, follow: { from } }) => (windows.push(time - from), { steps: [], influences: windows.length === 1 ? [{ what: "register", register: "r3", via: "value" }] : [wad("PLAYPAL")] });
  const graph = { address: 1, nodes: [], origins: [], from: { at: 1, time: 1e13 } };
  await depend(graph, { read: async () => ({}), query });
  assert.deepEqual(windows, [1e9, 1e11]);
  assert.equal(graph.origins[0].kind, "rom");
});

test("a strip of time spreads what was a microsecond before and what was a second before", () => {
  const svg = timeStrip([{ t: 1e9, kind: "step", label: "asked" }, { t: 1e9 - 1e3, kind: "step", label: "a µs" }, { t: 0, kind: "origin", label: "a s" }]);
  const xs = [...svg.matchAll(/<circle cx="([\d.]+)"/g)].map((m) => Number(m[1]));
  assert.equal(xs.length, 3);
  assert.ok(xs[0] > xs[1] && xs[1] > xs[2] && xs[0] - xs[1] > 100, `${xs}`);
  assert.equal(timeStrip([{ t: 1, kind: "step", label: "one" }]), "");
});
