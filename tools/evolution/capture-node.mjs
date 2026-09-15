// Prepare a validated immutable continuation for repeated browser/Node trials.
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { demo } from './browser-demo.mjs';

const [templatePath, output, members = '22', baselinePath] = process.argv.slice(2);
if (!templatePath || !output) throw new Error('Usage: node tools/evolution/capture-node.mjs TEMPLATE OUTPUT_DIRECTORY [REGION_MEMBERS] [BASELINE]');
await mkdir(output, { recursive: true });
// Node's eager-Wasm diagnostic modes can otherwise exit with an unsettled await.
const keepAlive = setInterval(() => {}, 1000);
try {
  await demo({
    compiler: await readFile('benchmark-results/browser-compiler.wasm'),
    template: await readFile(templatePath),
    baseline: baselinePath ? await readFile(baselinePath) : undefined,
    regionMembers: Number(members), log: console.log,
    async onArtifact({ wasm, baselineArtifact, ...manifest }) {
      await writeFile(join(output, 'successor.wasm'), wasm);
      await writeFile(join(output, 'manifest.json'), JSON.stringify({
        version: 1, wasm: './successor.wasm', templatePath, ...manifest,
      }, null, 2));
      if (baselineArtifact) {
        const { wasm, warmupTarget, warmupRetired, ...state } = baselineArtifact;
        await writeFile(join(output, 'baseline.wasm'), wasm);
        await writeFile(join(output, 'baseline.json'), JSON.stringify({
          version: 1, kind: 'warmup', wasm: './baseline.wasm', warmupTarget, ...state,
          measurement: { baselinePath, warmupRetired,
            hashes: { successor: manifest.measurement.hashes.baseline } },
        }, null, 2));
      }
    },
  });
} finally { clearInterval(keepAlive); }
