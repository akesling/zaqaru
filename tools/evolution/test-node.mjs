import { readFile, writeFile } from 'node:fs/promises';
import { demo } from './browser-demo.mjs';
const result = await demo({
  compiler: await readFile('benchmark-results/browser-compiler.wasm'),
  template: await readFile('benchmark-results/evolution-template.wasm'),
  baseline: await readFile('benchmark-results/evolution-baseline.wasm'),
  log: console.log,
  regionMembers: process.argv[2] === undefined ? undefined : Number(process.argv[2]),
});
await writeFile(`benchmark-results/evolution-node-${result.stdout.split(' ')[0]}.json`, JSON.stringify(result, null, 2));
await writeFile(`benchmark-results/evolution-node-${result.stdout.split(' ')[0]}-${result.totalRetired}.json`, JSON.stringify(result, null, 2));
