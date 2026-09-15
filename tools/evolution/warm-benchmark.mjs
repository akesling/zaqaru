// Portable benchmark: compilation once, fresh continuation for every execution.
// Never run disposable warmups against live external resources.
import { Container, MountTable, text } from '../../web/zaqaru.js';

function check(condition, message) { if (!condition) throw new Error(message); }
export function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}
export async function sha256(bytes) {
  return Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)))
    .map(b => b.toString(16).padStart(2, '0')).join('');
}

async function execute(variant) {
  const begin = performance.now();
  const container = await Container.instantiate(variant.module, MountTable.load(variant.manifest.mounts));
  const instantiateMs = performance.now() - begin;
  const ex = container.instance.exports;
  if (variant.manifest.kind === 'warmup') {
    check(ex.zaqaru_run(BigInt(variant.manifest.warmupTarget)) === 0, 'baseline ended during warmup');
    check(container.value('statistics').retired === variant.manifest.measurement.warmupRetired, 'baseline warmup drift');
  } else {
    check(ex.zaqaru_run(-1n) === 3, 'continuation was not frozen');
    check(ex.zaqaru_resume(1n) === 0, 'continuation resume failed');
  }
  const preparationMs = performance.now() - begin;
  const start = performance.now();
  const status = ex.zaqaru_run(-1n);
  const executionMs = performance.now() - start;
  const stdout = text(container.mounts.readback(['iso', 'console', 'stdout']) ?? new Uint8Array());
  const expected = variant.manifest.expected;
  check(status === expected.status && stdout === expected.stdout, 'exit status or output mismatch');
  check(container.value('statistics').retired === expected.totalRetired, 'retirement mismatch');
  check(JSON.stringify(container.value('processes')) === JSON.stringify(expected.processes), 'process state mismatch');
  for (const [key, name] of [['compiledRetired', 'zaqaru_compiled_retired'],
    ['regionRetired', 'zaqaru_region_retired'], ['regionEntries', 'zaqaru_region_entries']]) {
    if (variant.manifest.kind !== 'warmup') check(ex[name]().toString() === expected[key], `${key} mismatch`);
  }
  return { instantiateMs, preparationMs, executionMs };
}

export async function benchmark(artifacts, { warmups = 5, samples = 7, log = () => {}, profile } = {}) {
  check(Number.isInteger(warmups) && warmups >= 1 && warmups <= 100, 'warmups must be 1..100');
  check(Number.isInteger(samples) && samples >= 3 && samples <= 100, 'samples must be 3..100');
  check(artifacts.length >= 1 && artifacts.length <= 4, 'provide 1..4 artifacts');
  const variants = [];
  for (const { name, bytes, manifest } of artifacts) {
    check(manifest.version === 1, 'unknown artifact manifest version');
    check(manifest.kind === undefined || manifest.kind === 'warmup', 'unknown artifact kind');
    check(Number.isSafeInteger(manifest.expected.totalRetired) &&
      Number.isSafeInteger(manifest.measurement.warmupRetired) &&
      manifest.expected.totalRetired > manifest.measurement.warmupRetired, 'invalid retired work');
    // Local snapshots only: replay must not publish requests or mutate live I/O.
    check(manifest.mounts.mounts.every(m => ['sink', 'clock', 'shutdown', 'server'].includes(m.state.kind)),
      'benchmark requires disposable local mounts');
    check(await sha256(bytes) === manifest.measurement.hashes.successor, 'artifact hash mismatch');
    if (manifest.kind !== 'warmup') check(BigInt(manifest.expected.compiledRetired) > 0n, 'artifact did not exercise generated code');
    const begin = performance.now();
    const module = await WebAssembly.compile(bytes);
    const compileMs = performance.now() - begin;
    check(JSON.stringify(WebAssembly.Module.imports(module).map(i => `${i.module}.${i.name}`).sort()) ===
      JSON.stringify(['env.ll_read', 'env.ll_write']), 'executor imports exceeded the store ABI');
    variants.push({ name, module, manifest, compileMs, runs: [] });
  }
  for (const variant of variants.slice(1)) {
    const a = variants[0].manifest, b = variant.manifest;
    check(a.expected.stdout === b.expected.stdout && a.expected.totalRetired === b.expected.totalRetired,
      'artifacts did not execute the same workload');
    check(Math.abs(a.measurement.warmupRetired - b.measurement.warmupRetired) <= 100100,
      'artifact warmup drift exceeds one scheduler quantum');
  }
  for (let round = 0; round < 1 + warmups + samples; round++) {
    const phase = round === 0 ? 'first' : round <= warmups ? 'warmup' : 'sample';
    const order = round % 2 ? [...variants].reverse() : variants;
    for (const variant of order) {
      const run = { round, phase, ...await execute(variant) };
      variant.runs.push(run);
      log({ name: variant.name, ...run });
      // Permit task processing and background compiler progress between calls.
      await new Promise(resolve => setTimeout(resolve, 0));
    }
  }
  // Profiling is separate: attaching a profiler can change the compiler tier.
  if (profile) for (const variant of variants) {
    await profile(variant.name, () => execute(variant));
  }
  const results = variants.map(({ name, compileMs, manifest, runs }) => ({
    name, compileMs, hash: manifest.measurement.hashes.successor,
    remainingRetired: manifest.expected.totalRetired - manifest.measurement.warmupRetired,
    origin: manifest.measurement, expected: manifest.expected, runs,
    medianExecutionMs: median(runs.filter(r => r.phase === 'sample').map(r => r.executionMs)),
    medianInstantiationMs: median(runs.filter(r => r.phase === 'sample').map(r => r.instantiateMs)),
  }));
  return { engine: globalThis.navigator?.userAgent ?? 'unknown', warmups, samples, variants: results,
    pairedSpeedups: results.slice(1).map(candidate => {
      const controlRuns = results[0].runs.filter(r => r.phase === 'sample');
      const ratios = candidate.runs.filter(r => r.phase === 'sample')
        .map((r, i) => (controlRuns[i].executionMs / results[0].remainingRetired) /
          (r.executionMs / candidate.remainingRetired));
      return { control: results[0].name, candidate: candidate.name, ratios, median: median(ratios),
        normalizedForRetirement: results[0].remainingRetired !== candidate.remainingRetired };
    }),
    note: 'Fresh validated runs share one compiled module per variant. Baselines boot to their recorded warmup boundary outside the execution timer; successors resume a frozen continuation. Warm timings exclude module compilation, instance preparation, weval generation and disposable warmups. First-use timing may include lazy compilation. Paired ratios normalize for up to one scheduler quantum of retirement drift. Warm does not certify any particular compiler tier. Profiling runs separately. No general OCI speedup claim.' };
}
