// How far a re-execution has got, in words and as a bar: what a history,
// an audit, a value's story and a stand in the board's time show while the
// worker runs the board again from a checkpoint to the instant asked for.
//
// A report is the worker's (`reporter` in worker.js): the board's time at
// the checkpoint it began from (`start`), where following or watching
// begins (`from`, or null), the instant (`to`), where the board has got to
// (`board`), all in picoseconds of the board's time, and the milliseconds
// it has taken (`elapsed`).

/// A time of the board's, in picoseconds, as the page writes one.
export function boardTime(ps) {
  const s = ps / 1e12;
  return s >= 1 ? `${s.toFixed(3)} s` : `${(ps / 1e9).toFixed(3)} ms`;
}

/// A wait in milliseconds of the wall's, roughly.
export function wait(ms) {
  if (ms < 1500) return "a moment";
  if (ms < 90_000) return `${Math.round(ms / 1000)} s`;
  return `${Math.round(ms / 60_000)} min`;
}

/// `{ fraction, phase, left }`: how much of the run is done, from 0 to 1;
/// what it is doing, in words; and roughly how long it has left, in words,
/// once there is enough of it done to say.
export function progress({ start, from, to, board, elapsed }) {
  const span = Math.max(1, to - start);
  const fraction = Math.min(1, Math.max(0, (board - start) / span));
  const phase =
    from !== null && from !== undefined && board >= from
      ? `following from ${boardTime(from)}: at ${boardTime(board)} of ${boardTime(to)}`
      : `running the board again from its checkpoint at ${boardTime(start)}: at ${boardTime(board)} of ${boardTime(to)}`;
  const left = fraction >= 0.02 && elapsed >= 500 ? `about ${wait((elapsed * (1 - fraction)) / fraction)} left` : null;
  return { fraction, phase, left };
}

/// A report as HTML: a bar, and what it is doing.
export function progressHtml(report) {
  if (!report) return `<div class="progress"><div class="bar"><span style="width:0%"></span></div><span class="dim">restoring a checkpoint…</span></div>`;
  const { fraction, phase, left } = progress(report);
  return `<div class="progress"><div class="bar"><span style="width:${(100 * fraction).toFixed(1)}%"></span></div><span class="dim">${phase}${left ? ` · ${left}` : ""}</span></div>`;
}
