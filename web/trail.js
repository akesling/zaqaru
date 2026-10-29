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
/// bytes it was held in back to the write that put them there (`{ at,
/// time }`), run the window again wider (`{ widen }`), or end (`{ end }`):
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
      if (!last.ram) return { end: "rom" };
      if (!last.written) return { end: "unwritten" };
      if (last.written.by?.pc === undefined) return { end: "writer" };
      // The stamp is the write's microsecond plus one: its end.
      return { at: last.addr, time: last.written.stamp * 1e6 };
    default:
      return { end: "unfollowed" };
  }
}

/// Follows `trail.address` back, filling `trail.hops` — `{ at, time,
/// window, what, steps }`, latest first — and then `trail.end`. Begins at
/// the last write to it as things stand (`read`), or at `trail.from` (`{
/// at, time }`) to go on from where a story stopped. `query(options,
/// path)` reads from a re-execution; `update()` after each window; and it
/// stops between windows once `wanted()` says not.
export async function follow(trail, { read, query, update = () => {}, wanted = () => true, hops = HOPS }) {
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
    const answer = await query({ time, follow: { from: Math.max(0, time - window) } }, `board/why/${at}`);
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
