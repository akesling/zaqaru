// The timeline's custom filter: which rows to show, by what each is about.
//
// Every row is reduced to fields — the process that made it, the syscall
// it is, the host path an exchange was on, the Hubris task and the kind of
// call a board's record is, and its text — and a filter is a value for any
// of them, a row shown when it has every value chosen. A field applies to
// the rows that have it: a syscall chosen leaves out exchanges and the
// board's records, which are no syscall; a Hubris task chosen leaves out
// the container's rows.
//
// The dropdowns offer what the timeline holds, not a list written down
// here: each one's options are counted from the rows as they arrive and
// made again as the run grows, most frequent first, with whatever is
// chosen kept even while nothing shown has it.

/// The fields a dropdown chooses, with what its "any" says.
export const FIELDS = [
  ["process", "any process"],
  ["syscall", "any syscall"],
  ["path", "any host path"],
  ["task", "any Hubris task"],
  ["call", "any Hubris call"],
];

export function emptySpec() {
  return { process: "", syscall: "", path: "", task: "", call: "", text: "" };
}

/// Whether nothing is chosen: every row shown.
export function isEmpty(spec) {
  return Object.values(spec).every((v) => !v);
}

/// A host path as the dropdown groups it: its segments up to the first
/// that names one of many (a connection's number, a pid), at most three —
/// `iso/net/conn`, `iso/time/monotonic_ns`.
export function pathGroup(path) {
  const parts = path.split("/").filter(Boolean);
  const out = [];
  for (const part of parts) {
    if (/^\d+$/.test(part) || out.length === 3) break;
    out.push(part);
  }
  return out.join("/");
}

/// Whether a row with these fields passes the filter.
export function matches(spec, fields) {
  for (const [name] of FIELDS) {
    if (!spec[name]) continue;
    const value = fields[name];
    if (value === undefined || value === null) return false;
    if (name === "path" ? !(value === spec.path || value.startsWith(`${spec.path}/`)) : value !== spec[name]) return false;
  }
  if (spec.text && !(fields.text ?? "").toLowerCase().includes(spec.text.toLowerCase())) return false;
  return true;
}

/// What each dropdown can offer, counted from the rows seen.
export class Options {
  constructor() {
    this.counts = Object.fromEntries(FIELDS.map(([name]) => [name, new Map()]));
  }

  note(fields) {
    for (const [name] of FIELDS) {
      let value = fields[name];
      if (value === undefined || value === null || value === "") continue;
      if (name === "path") value = pathGroup(value);
      this.counts[name].set(value, (this.counts[name].get(value) ?? 0) + 1);
    }
  }

  /// A dropdown's options: `[value, count]`, most frequent first, then by
  /// name; `chosen` among them even if no row has had it.
  list(name, chosen = "") {
    const out = [...this.counts[name]].sort((a, b) => b[1] - a[1] || String(a[0]).localeCompare(String(b[0])));
    if (chosen && !this.counts[name].has(chosen)) out.unshift([chosen, 0]);
    return out;
  }
}
