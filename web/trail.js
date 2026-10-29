// A value's story: where the bytes at an address came from, followed back
// through every copy and every write until it reaches where it came in.
//
// Each step back is a re-execution of the board over a short window that
// ends just after the write that put the value there, every value followed
// (`board/provenance` `everything`). The window's history of the bytes
// runs back through copies and stores to where the value came into the
// window: off a bus, out of flash or a NOR, made by the code itself — or
// held in RAM since before, with the last write to it, which is the next
// window's end. A value that sat in a register from before the window
// widens it and runs again.

/// The first window, in picoseconds of the board's time: 1 ms.
export const WINDOW = 1e9;
/// The widest a window is made for a value that sat in a register: 1 s.
export const WIDEST = 1e12;
/// How many windows back a story goes before it asks to go on.
export const HOPS = 16;

/// What the last step of a window's history says to do next: follow the
/// byte it was held in (`byte`, of the `addr` loaded) back to the write
/// that put it there (`{ at, time }`), run the window again wider (`{
/// widen }`), or end (`{ end }`):
/// `bus` off a peripheral, `rom` out of flash or a NOR or a register,
/// `constant` made by the code, `unwritten` never written, `writer` written
/// by no instruction — DMA, the probe — or `unfollowed`.
export function next(last) {
  switch (last?.what) {
    case "read":
      return { end: "bus" };
    case "register":
      return { widen: true };
    case "store":
      return { end: "constant" };
    case "held":
      // A peripheral's register: a status, a counter — from outside too.
      if (!last.ram && last.addr >= 0x4000_0000 && last.addr < 0x6000_0000) return { end: "bus" };
      if (!last.ram) return { end: "rom" };
      if (!last.written) return { end: "unwritten" };
      if (last.written.by?.pc === undefined) return { end: "writer" };
      // The stamp is the write's microsecond plus one: its end.
      return { at: last.byte ?? last.addr, time: last.written.stamp * 1e6 };
    default:
      return { end: "unfollowed" };
  }
}

/// Follows `trail.address` back, filling `trail.hops` — `{ at, time,
/// window, what, steps }`, latest first — and then `trail.end`. Begins at
/// the last write to it as things stand (`read`), or at `trail.from` (`{
/// at, time }`) to go on from where a story stopped. `query(options,
/// path)` reads from a re-execution; `update()` after each window; and it
/// stops between windows once `wanted()` says not. `cache`, a Map, keeps
/// each window's answer for the next story that runs the same one: the
/// board's past does not change.
export async function follow(trail, { read, query, update = () => {}, wanted = () => true, hops = HOPS, cache = null }) {
  let at, time;
  if (trail.from) {
    ({ at, time } = trail.from);
    trail.from = null;
  } else {
    const written = await read(`board/written/${trail.address}/1`);
    const [stamp, writer] = written?.words ?? [0, -1];
    const by = written?.writers?.[writer];
    if (!stamp) return (trail.end = { kind: "unwritten" });
    if (by?.pc === undefined) return (trail.end = { kind: "writer", by });
    [at, time] = [trail.address, stamp * 1e6];
  }
  let window = WINDOW;
  let taken = 0;
  while (wanted()) {
    if (taken >= hops) return (trail.end = { kind: "more", at, time });
    trail.running = { at, time, window };
    update();
    const key = `${at} ${time} ${window}`;
    const answer = cache?.get(key) ?? (await query({ time, follow: { from: Math.max(0, time - window) } }, `board/why/${at}`));
    cache?.set(key, answer);
    const steps = answer?.steps ?? [];
    const step = next(steps.at(-1));
    if (step.widen && window < WIDEST) {
      window *= 10;
      continue;
    }
    trail.hops.push({ at, time, window, what: answer?.what ?? null, steps });
    taken++;
    if (step.at === undefined) return (trail.end = { kind: step.end ?? "register", step: steps.at(-1) ?? null });
    ({ at, time } = step);
    window = WINDOW;
  }
  return null;
}

// ---- telling it ----------------------------------------------------------------

/// Code that moves bytes and makes nothing of them — memcpy and its kin,
/// the kernel's copies between tasks — by its function and file.
const PLUMBING = /^(memcpy|memmove|__aeabi_mem\w*|copy_(forward|backward)\w*|safe_copy\w*|copy_nonoverlapping|copy_from_slice|clone_from_slice)$|::(memcpy|memmove|copy_(forward|backward)\w*)$/;
const PLUMBING_FILES = /(^|\/)(mem\/impls\.rs|compiler_builtins\/|memcpy|memmove)/;

/// The source line a step's code is at: the innermost the compiler did
/// not make up (line 0 is code of its own, of no line).
export function lineOf(step) {
  return step.lines?.find((l) => l.line > 0) ?? step.lines?.[0] ?? null;
}

/// The function a step's code is in, by its source if known: `R_DrawSpan`.
export function functionOf(step) {
  const line = lineOf(step);
  const name = (line?.function ?? step.code ?? "").replace(/^\S+: /, "").replace(/ \+ 0x[0-9a-f]+$/, "").replace(/^.*::/, "");
  // What is left of a generic's angle brackets once its path is gone.
  return name.includes("<") ? name : name.replace(/>+$/, "");
}

/// Whose code a step's is, where the code says: `doom`, or null.
export function ownerOf(step) {
  return /^([\w-]+): /.exec(step.code ?? "")?.[1] ?? null;
}

/// Whose memory a step's place is, where it says: `display`, or null.
export function placeOwner(step) {
  return /^([\w-]+) (\.[\w.]+|stack|code)(:|$| \()/.exec(step.where ?? "")?.[1] ?? null;
}

/// Whether a step is a byte moved by plumbing: a store or a load by code
/// that only copies.
export function isPlumbing(step) {
  if (step.what !== "store" && step.what !== "held" && step.what !== "load") return false;
  const line = step.lines?.[0];
  return PLUMBING.test(functionOf(step)) || PLUMBING_FILES.test(line?.file ?? "");
}

/// A story's steps, latest first, across its windows, each with the
/// window it is of: `[{ step, hop }]`.
export function stepsOf(trail) {
  return trail.hops.flatMap((hop, h) => hop.steps.map((step) => ({ step, hop: h })));
}

/// The steps as what to show: runs of plumbing folded into one `{ copies:
/// [{ step, hop }] }` each, every other step `{ step, hop }` as it is.
export function folded(steps) {
  const out = [];
  for (const entry of steps) {
    if (isPlumbing(entry.step)) {
      const last = out.at(-1);
      if (last?.copies) last.copies.push(entry);
      else out.push({ copies: [entry] });
    } else out.push(entry);
  }
  // A lone load or store by plumbing says nothing folded.
  return out.map((entry) => (entry.copies?.length === 1 ? entry.copies[0] : entry));
}

/// A span of the board's time, in picoseconds, in words.
export function span(ps) {
  ps = Math.abs(ps);
  if (ps < 1e6) return `${Math.round(ps / 1e3)} ns`;
  if (ps < 1e9) return `${+(ps / 1e6).toPrecision(3)} µs`;
  if (ps < 1e12) return `${+(ps / 1e9).toPrecision(3)} ms`;
  return `${+(ps / 1e12).toPrecision(3)} s`;
}

/// The byte a step is about, of the value it moved.
export function byteOf(step) {
  if (step.value === undefined) return null;
  const lane = step.size > 1 && step.byte - step.addr >= 0 && step.byte - step.addr < step.size ? step.byte - step.addr : 0;
  return (step.value >>> (8 * lane)) & 0xff;
}

/// Where a story's value came from, in words, from how it ended.
export function originText(trail) {
  const end = trail.end;
  const last = end?.step;
  switch (end?.kind) {
    case "bus":
      return `came in off ${last.register}${last.record ? ` (trace record ${last.record.n})` : ""}`;
    case "rom":
      return `came from ${last.where}`;
    case "constant":
      return `is a constant the code made`;
    case "unwritten":
      return `has never been written: it is what the RAM held at reset`;
    case "writer":
      return `was written by ${end.by?.code ?? last?.written?.by?.code ?? "something not an instruction"}`;
    case "register":
      return `sat in ${last?.register ?? "a register"} longer than the widest window`;
    case "unfollowed":
      return `was written by something not followed (the probe, or DMA)`;
    default:
      return null;
  }
}

/// A place as a story says it: the static and what in it, without whose
/// it is (said beside it), the memory's own name (said on hover) or the
/// modules it is in — `ZONE_AXI[55416]`, `WAD lump COLORMAP + 0x165b: …`.
export function shortPlace(where) {
  return (where ?? "")
    .replace(/^[\w-]+ (\.[\w.]+|stack|code): /, "")
    .replace(/ \([A-Za-z][\w ]*( \+ 0x[0-9a-f]+)?\)$/, "")
    .replace(/\b(\w+::)+/g, "");
}

/// One of the values an address was worked out from, in a few words: its
/// value and where it was.
export function leafText(leaf) {
  const where = shortPlace(leaf.where);
  switch (leaf.what) {
    case "held":
      return `${byteOf(leaf) === null ? "" : `0x${byteOf(leaf).toString(16)} `}from ${where}`;
    case "read":
      return `0x${leaf.value.toString(16)} off ${leaf.register}`;
    case "register":
      return `what ${leaf.register} held`;
    default:
      return "a constant";
  }
}

/// A story in one sentence: where the value came from, what made it, and
/// how it got here.
export function summary(trail) {
  const steps = stepsOf(trail).map((e) => e.step);
  const origin = originText(trail);
  if (!steps.length || !origin) return null;
  const value = byteOf(steps[0]);
  const head = value === null ? "It" : `0x${value.toString(16).padStart(2, "0")}`;
  // The oldest store by code that is not plumbing: what made the value.
  const made = [...steps].reverse().find((s) => s.what === "store" && !isPlumbing(s));
  const parts = [`${head} ${origin}`];
  if (made) {
    const line = lineOf(made);
    const where = line?.file ? ` (${line.file.split("/").pop()}:${line.line})` : "";
    const owner = ownerOf(made);
    parts.push(`was stored by ${functionOf(made)}${where}${owner ? ` in ${owner}` : ""}`);
  }
  // What chose the value: the values the address it was looked up at was
  // worked out from, at the lookup nearest what made it.
  const lookup = steps.find((s) => s.what === "load" && !isPlumbing(s) && (!made || s.t <= made.t));
  if (lookup?.address?.length) parts.push(`looked up by ${lookup.address.map(leafText).join(" and ")}`);
  const copies = steps.filter((s) => s.what === "store" && isPlumbing(s) && (!made || s.t > made.t));
  if (copies.length) {
    const into = placeOwner(steps[0]);
    const later = made ? `, ${span((steps[0].t - made.t) * 1000)} later` : "";
    parts.push(`and was copied ${copies.length === 1 ? "once" : `${copies.length} times`}${into ? ` into ${into}` : ""}${later}`);
  }
  return `${parts.join(", ")}.`;
}
