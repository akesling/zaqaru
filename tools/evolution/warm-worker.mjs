import { benchmark } from './warm-benchmark.mjs';
try {
  const params = new URL(location.href).searchParams;
  const manifests = JSON.parse(params.get('manifests'));
  const artifacts = [];
  for (const path of manifests) {
    const url = new URL(path, location.href);
    const response = await fetch(url);
    if (!response.ok) throw new Error(`manifest fetch: ${response.status} ${path}`);
    const manifest = await response.json();
    const wasm = await fetch(new URL(manifest.wasm, url));
    if (!wasm.ok) throw new Error(`Wasm fetch: ${wasm.status} ${path}`);
    artifacts.push({ name: path, manifest, bytes: new Uint8Array(await wasm.arrayBuffer()) });
  }
  const result = await benchmark(artifacts, {
    warmups: Number(params.get('warmups') ?? 5), samples: Number(params.get('samples') ?? 7),
    log: message => postMessage({ message: JSON.stringify(message) }),
  });
  postMessage({ result });
} catch (error) { postMessage({ error: String(error), stack: error?.stack }); }
