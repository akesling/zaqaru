import { demo } from './browser-demo.mjs';
try {
  const regionMembers = new URL(location.href).searchParams.get('regionMembers');
  const read = async path => new Uint8Array(await (await fetch(path)).arrayBuffer());
  const result = await demo({
    compiler: await read('../../benchmark-results/browser-compiler.wasm'),
    template: await read('../../benchmark-results/evolution-template.wasm'),
    baseline: await read('../../benchmark-results/evolution-baseline.wasm'),
    log: message => postMessage({ message }),
    regionMembers: regionMembers === null ? undefined : Number(regionMembers),
  });
  postMessage({ result });
} catch (error) {
  postMessage({ error: String(error), stack: error?.stack });
}
