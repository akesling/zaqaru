// The board's code, indexed (`board/index`): from a name to where it was
// defined, for hopping about the source the way an editor does — a name
// in the code clicked to go to it, or looked for by what it is called.

/// `board/index`'s answer as `{ symbols, byName }`: each symbol `{ name,
/// kind, file, line, path, declared }`, and them by name.
export function readIndex(answer) {
  const files = answer?.files ?? [];
  const symbols = (answer?.symbols ?? []).map(([name, kind, file, line, path, declared]) => ({ name, kind, file: files[file], line, path: path ?? name, declared: !!declared }));
  const byName = new Map();
  for (const s of symbols) {
    if (!byName.has(s.name)) byName.set(s.name, []);
    byName.get(s.name).push(s);
  }
  return { symbols, byName };
}

/// How many leading directories two paths share.
function shared(a, b) {
  const x = a.split("/");
  const y = b.split("/");
  let n = 0;
  while (n < x.length - 1 && n < y.length - 1 && x[n] === y[n]) n++;
  return n;
}

/// Where `name`, as it is written in `file`, is likely defined, likeliest
/// first: defined before only declared, then in the same file, then the
/// nearest by directory — the same crate, the same task's C.
export function definitionsOf(index, name, file = "") {
  const all = index?.byName.get(name) ?? [];
  const score = (s) => [s.declared ? 1 : 0, s.file === file ? 0 : 1, -shared(s.file, file)];
  return [...all].sort((a, b) => {
    const [p, q] = [score(a), score(b)];
    for (let i = 0; i < p.length; i++) if (p[i] !== q[i]) return p[i] - q[i];
    return a.file.localeCompare(b.file) || a.line - b.line;
  });
}

/// The symbols `query` names, best first: its name exactly, then a name
/// it begins, then one it is in, then a path it is in — `Task::new`
/// finds `kern::task::Task::new`. Case matters only when the query has
/// capitals. Only declared, where defined too, left out.
export function search(index, query, most = 30) {
  const q = query.trim();
  if (!q || !index) return [];
  const fold = q === q.toLowerCase() ? (s) => s.toLowerCase() : (s) => s;
  const ranked = [];
  for (const s of index.symbols) {
    const name = fold(s.name);
    let rank;
    if (name === q) rank = 0;
    else if (name.startsWith(q)) rank = 1;
    else if (name.includes(q)) rank = 2;
    else if (q.includes("::") && fold(s.path).includes(q)) rank = 3;
    else continue;
    ranked.push([rank, s.declared ? 1 : 0, s.name.length, s]);
  }
  ranked.sort((a, b) => a[0] - b[0] || a[1] - b[1] || a[2] - b[2] || a[3].name.localeCompare(b[3].name));
  const out = [];
  const seen = new Set();
  for (const [, , , s] of ranked) {
    if (s.declared && index.byName.get(s.name).some((d) => !d.declared && d.path === s.path)) continue;
    const key = `${s.path} ${s.file}:${s.line}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(s);
    if (out.length >= most) break;
  }
  return out;
}

/// A file as a list of them says it: its last two parts.
export function shortFile(file) {
  return file.split("/").slice(-2).join("/");
}
