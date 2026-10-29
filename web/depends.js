// Everything a value depended on: its story's way back, and every other
// value that went into it or chose it, each followed back in turn — write
// by write, window by window — to where it came in.
//
// Each window is a re-execution following everything (`board/influences`),
// as a story's is (see trail.js). A value held in RAM since before the
// window is followed on from the write that put it there, in a window of
// its own; one in a register widens its window; the rest are where values
// came in — off a bus, out of flash or a NOR, made by the code. The walk
// goes breadth first, nearest first, until it has run `budget` windows,
// and says what it left unexplored.

import { WIDEST, WINDOW, functionOf, next, shortPlace } from "./trail.js";

/// Walks back from `graph.address`, filling `graph.nodes` — `{ id, at,
/// time, window, depth, via, parent, what, steps }`, the value at `at` as
/// written at `time` — and `graph.origins` — `{ kind, leaf, node, via }`,
/// where each value came in — then `graph.left`, how many it did not get
/// to. Begins at the write `graph.from` (`{ at, time }`), or the last to
/// the address as things stand (`read`). `query`, `update`, `wanted` and
/// `cache` as `follow`'s in trail.js.
export async function depend(graph, { read, query, update = () => {}, wanted = () => true, budget = 60, cache = null }) {
  // Going on from where it stopped: what it had yet to follow.
  if (graph.queue) return walk(graph, { query, update, wanted, budget, cache });
  let start = graph.from;
  if (!start) {
    const written = await read(`board/written/${graph.address}/1`);
    const [stamp, writer] = written?.words ?? [0, -1];
    const by = written?.writers?.[writer];
    if (!stamp || by?.pc === undefined) {
      graph.origins.push({ kind: stamp ? "writer" : "unwritten", leaf: { what: "held", where: null, written: stamp ? { by } : null }, node: null, via: "value" });
      return graph;
    }
    start = { at: graph.address, time: stamp * 1e6 };
  }
  graph.queue = [{ ...start, depth: 0, via: "value", parent: null }];
  graph.seen = new Set([`${start.at} ${start.time}`]);
  return walk(graph, { query, update, wanted, budget, cache });
}

async function walk(graph, { query, update, wanted, budget, cache }) {
  const { queue, seen } = graph;
  let runs = 0;
  while (queue.length && runs < budget && wanted()) {
    const item = queue.shift();
    let window = WINDOW;
    let answer;
    for (;;) {
      const key = `influences ${item.at} ${item.time} ${window}`;
      graph.running = { ...item, window };
      update();
      answer = cache?.get(key) ?? (await query({ time: item.time, follow: { from: Math.max(0, item.time - window) } }, `board/influences/${item.at}`));
      cache?.set(key, answer);
      runs++;
      // A value that sat in a register from before: a wider window, once.
      const sat = (answer?.influences ?? []).some((leaf) => leaf.what === "register");
      if (!sat || window >= WIDEST || runs >= budget) break;
      window = Math.min(WIDEST, window * 100);
    }
    const node = { id: graph.nodes.length, ...item, window, what: answer?.what ?? null, steps: answer?.steps ?? [] };
    graph.nodes.push(node);
    for (const leaf of answer?.influences ?? []) {
      // Whether it chose the value it was found in, rather than being it.
      const via = leaf.via;
      const step = next(leaf);
      if (step.at !== undefined) {
        const key = `${step.at} ${step.time}`;
        if (seen.has(key)) continue;
        seen.add(key);
        queue.push({ at: step.at, time: step.time, depth: item.depth + 1, via, parent: node.id, leaf });
      } else graph.origins.push({ kind: step.widen ? "register" : step.end, leaf, node: node.id, via });
    }
    update();
  }
  graph.left = queue.length;
  graph.running = null;
  return graph;
}

/// What a place is, for grouping: its static or lump, without which
/// element or how far in — `WAD lump COLORMAP`, `ZONE_AXI`, `USART3 RDR`.
export function family(origin) {
  const leaf = origin.leaf;
  if (origin.kind === "bus") return leaf.register ?? (shortPlace(leaf.where ?? "").replace(/\s\+\s0x[0-9a-f]+.*$/, "") || "a peripheral");
  if (origin.kind === "constant") return `made in ${functionOf(leaf) || "the code"}`;
  const place = shortPlace(leaf.where ?? "");
  return place.replace(/(\[\d+\]|\s\+\s0x[0-9a-f]+|, entry \d+).*$/, "") || "somewhere";
}

const KINDS = [
  ["bus", "from outside"],
  ["rom", "from data"],
  ["constant", "made by the code"],
  ["unwritten", "never written"],
  ["writer", "written by no instruction"],
  ["register", "in a register from before"],
  ["unfollowed", "not followed"],
];

/// The origins grouped: by kind, in the order above, each kind's places by
/// family, most first — `[{ kind, label, families: [{ name, origins }] }]`.
export function grouped(graph) {
  return KINDS.map(([kind, label]) => {
    const families = new Map();
    for (const origin of graph.origins.filter((o) => o.kind === kind)) {
      const name = family(origin);
      if (!families.has(name)) families.set(name, []);
      families.get(name).push(origin);
    }
    return { kind, label, families: [...families].map(([name, origins]) => ({ name, origins })).sort((a, b) => b.origins.length - a.origins.length) };
  }).filter((g) => g.families.length);
}

/// What a value depended on, in a sentence.
export function dependsSummary(graph, most = 5) {
  const groups = grouped(graph);
  if (!groups.length) return null;
  const parts = groups.map((g) => {
    const named = g.families.slice(0, most).map((f) => (f.origins.length > 1 ? `${f.name} ×${f.origins.length}` : f.name));
    const rest = g.families.length - named.length;
    return `${g.label}: ${named.join(", ")}${rest > 0 ? ` and ${rest} more` : ""}`;
  });
  const writes = graph.nodes.length;
  const head = `It depended on ${graph.origins.length} value${graph.origins.length === 1 ? "" : "s"}, followed back through ${writes} write${writes === 1 ? "" : "s"}`;
  return `${head} — ${parts.join("; ")}${graph.left ? `; ${graph.left} more not yet followed` : ""}.`;
}

/// The chain of places from the value asked about to `node`, oldest last.
export function pathTo(graph, id) {
  const out = [];
  for (let node = graph.nodes[id]; node; node = node.parent === null ? null : graph.nodes[node.parent]) out.push(node);
  return out;
}

/// A strip of time, as SVG: `points` — `{ t, kind, label }`, in nanoseconds
/// of the board's — laid out from the earliest at the left to the latest
/// at the right, on a scale of how long before the latest, so that what
/// was a microsecond before and what was a second before are both seen.
export function timeStrip(points, width = 640, height = 34) {
  if (points.length < 2) return "";
  const latest = Math.max(...points.map((p) => p.t));
  const earliest = Math.min(...points.map((p) => p.t));
  const scale = (t) => Math.log10(1 + (latest - t)) / Math.log10(1 + Math.max(1, latest - earliest));
  const x = (t) => 8 + (width - 16) * (1 - scale(t));
  const ticks = [1e3, 1e6, 1e9]
    .filter((ns) => ns < latest - earliest)
    .map((ns) => `<line x1="${x(latest - ns).toFixed(1)}" x2="${x(latest - ns).toFixed(1)}" y1="6" y2="22" class="tick"/><text x="${x(latest - ns).toFixed(1)}" y="32" class="tick">${ns === 1e3 ? "1 µs" : ns === 1e6 ? "1 ms" : "1 s"} before</text>`)
    .join("");
  const dots = points
    .map((p) => `<circle cx="${x(p.t).toFixed(1)}" cy="14" r="${p.kind === "origin" ? 4 : 3}" class="${p.kind}"><title>${p.label}</title></circle>`)
    .join("");
  return `<svg class="time-strip" viewBox="0 0 ${width} ${height}" width="${width}" height="${height}"><line x1="8" x2="${width - 8}" y1="14" y2="14" class="axis"/>${ticks}${dots}</svg>`;
}
