import { readFile, writeFile } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { Session } from 'node:inspector/promises';
import { benchmark } from './warm-benchmark.mjs';

const args = process.argv.slice(2);
const output = args.shift();
const manifests = args.filter(arg => !arg.startsWith('--'));
const allowed = /^(--warmups=[0-9]+|--samples=[0-9]+|--profile)$/;
if (!output || !manifests.length || args.some(arg => arg.startsWith('--') && !allowed.test(arg))) {
  throw new Error('Usage: node [V8_FLAGS] tools/evolution/warm-node.mjs OUTPUT.json MANIFEST... [--warmups=5] [--samples=7] [--profile]');
}
const number = (name, fallback) => Number(args.find(arg => arg.startsWith(`--${name}=`))?.split('=')[1] ?? fallback);
const artifacts = [];
for (const path of manifests) {
  const manifest = JSON.parse(await readFile(path, 'utf8'));
  artifacts.push({ name: path, manifest, bytes: await readFile(resolve(dirname(path), manifest.wasm)) });
}
const keepAlive = setInterval(() => {}, 1000);
const profiles = [];
try {
  const result = await benchmark(artifacts, {
    warmups: number('warmups', 5), samples: number('samples', 7),
    log: value => console.log(JSON.stringify(value)),
    profile: args.includes('--profile') ? async (name, run) => {
      const session = new Session();
      session.connect();
      try {
        await session.post('Profiler.enable');
        await session.post('Profiler.setSamplingInterval', { interval: 100 });
        await session.post('Profiler.start');
        for (let i = 0; i < 10; i++) await run();
        const { profile } = await session.post('Profiler.stop');
        const path = `${output}.${profiles.length}.cpuprofile`;
        await writeFile(path, JSON.stringify(profile));
        profiles.push({ name, path });
      } finally { session.disconnect(); }
    } : undefined,
  });
  await writeFile(output, JSON.stringify({ node: process.version, v8: process.versions.v8,
    flags: process.execArgv, profiles, ...result }, null, 2));
} finally { clearInterval(keepAlive); }
