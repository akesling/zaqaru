// Alternate saved templates through the same compiler, workload and host.
import { readFile, writeFile } from 'node:fs/promises';
import { demo } from './browser-demo.mjs';

const [beforePath, afterPath, outputPath, count = '3', regionSize, beforeRegionSize] = process.argv.slice(2);
if (!beforePath || !afterPath || !outputPath || !/^[1-9][0-9]*$/.test(count)) {
  throw new Error('Usage: node tools/evolution/compare-node.mjs BEFORE.wasm AFTER.wasm OUTPUT.json [REPEATS] [AFTER_REGION_MEMBERS] [BEFORE_REGION_MEMBERS]');
}
const compiler = await readFile('benchmark-results/browser-compiler.wasm');
const baseline = await readFile('benchmark-results/evolution-baseline.wasm');
const templates = { before: await readFile(beforePath), after: await readFile(afterPath) };
const samples = [];
const median = values => {
  const sorted = [...values].sort((a, b) => a - b);
  return (sorted[Math.floor((sorted.length - 1) / 2)] + sorted[Math.floor(sorted.length / 2)]) / 2;
};
for (let repeat = 0; repeat < Number(count); repeat++) {
  const pair = {};
  for (const variant of repeat % 2 ? ['after', 'before'] : ['before', 'after']) {
    const members = variant === 'after' ? regionSize : beforeRegionSize;
    const result = await demo({ compiler, baseline, template: templates[variant], log: () => {},
      regionMembers: members === undefined ? undefined : Number(members) });
    samples.push({ repeat, variant, ...result });
    pair[variant] = result;
    // Preserve completed samples even if a later execution fails.
    await writeFile(outputPath, JSON.stringify({ beforePath, afterPath, samples }, null, 2));
  }
  if (pair.before.stdout !== pair.after.stdout || pair.before.totalRetired !== pair.after.totalRetired) {
    throw new Error('Templates did not execute the same workload');
  }
  const remaining = result => Number(result.totalRetired) - result.warmupRetired;
  const speedup = (pair.before.successorMs / remaining(pair.before)) /
    (pair.after.successorMs / remaining(pair.after));
  pair.after.pairedSpeedup = speedup;
  samples.find(s => s.repeat === repeat && s.variant === 'after').pairedSpeedup = speedup;
  console.log(JSON.stringify({ repeat, beforeMs: pair.before.successorMs, afterMs: pair.after.successorMs,
    speedup, regionRetired: pair.after.regionRetired, regionEntries: pair.after.regionEntries }));
}
const result = { beforePath, afterPath,
  medianPairedSpeedup: median(samples.filter(s => s.variant === 'after').map(s => s.pairedSpeedup)), samples };
await writeFile(outputPath, JSON.stringify(result, null, 2));
console.log(`Median paired tail speedup: ${result.medianPairedSpeedup.toFixed(3)}x (transition excluded)`);
