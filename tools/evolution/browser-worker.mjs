import { demo } from './browser-demo.mjs';
try {
  const read = async path => new Uint8Array(await (await fetch(path)).arrayBuffer());
  const result = await demo({
    compiler: await read('../../benchmark-results/browser-compiler.wasm'),
    template: await read('../../benchmark-results/evolution-template.wasm'),
    baseline: await read('../../benchmark-results/evolution-baseline.wasm'),
    log: message => postMessage({ message }),
  });
  postMessage({ result });
} catch (error) {
  postMessage({ error: String(error), stack: error?.stack });
}
