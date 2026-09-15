// Runs unchanged in a browser Worker and Node. No native compiler or server RPC.
import { Container, standardMounts, toBase64, fromBase64, text } from '../../web/zaqaru.js';
import { instantiate } from '../../benchmark-results/structfs/featherweight/host/browser/dist/structfs-host.js';

function check(condition, message) { if (!condition) throw new Error(message); }

export async function optimizerRequest(compiler, path, data) {
  const module = await WebAssembly.compile(compiler);
  check(JSON.stringify(WebAssembly.Module.imports(module).map(i => `${i.module}.${i.name}`).sort()) ===
    JSON.stringify(['structfs.read', 'structfs.write']), 'optimizer imports exceeded the store ABI');
  const requests = [{ op: 'write', path, data, respond_to: 'iso/responses/write' }];
  let answer, error;
  const guest = await instantiate(compiler, {
    read(path) {
      check(path === 'iso/server/requests', `unexpected optimizer read: ${path}`);
      return requests.shift() ?? null;
    },
    write(path, value) {
      if (path === 'iso/responses/write') {
        if (value.result === 'error') error = value.error.message;
        else requests.push({ op: 'read', path: value.path, respond_to: 'iso/responses/read' });
      } else if (path === 'iso/responses/read') {
        if (value.result === 'error') error = value.error.message;
        else answer = value.value;
      } else if (path === 'iso/log/error') error ??= value;
      else check(path === 'iso/shutdown/complete', `unexpected optimizer write: ${path}`);
      return path;
    },
  });
  let code;
  try { code = guest.run(); }
  catch (cause) { throw new Error(`optimizer trapped: ${error ?? cause.message}`, { cause }); }
  check(code === 0 && !error && answer, `optimizer failed: ${error ?? code}`);
  return fromBase64(answer.wasm);
}

function sparse(memory) {
  const pages = [];
  const words = new Uint32Array(memory.buffer, memory.byteOffset, memory.byteLength / 4);
  for (let offset = 0; offset < memory.length; offset += 65536) {
    const end = Math.min((offset + 65536) / 4, words.length);
    for (let word = offset / 4; word < end; word++) {
      if (words[word] !== 0) {
        pages.push([offset, toBase64(memory.subarray(offset, offset + 65536))]);
        break;
      }
    }
  }
  return pages;
}

async function executor(bytes, mounts) {
  const module = await WebAssembly.compile(bytes);
  check(JSON.stringify(WebAssembly.Module.imports(module).map(i => `${i.module}.${i.name}`).sort()) ===
    JSON.stringify(['env.ll_read', 'env.ll_write']), 'executor imports exceeded the store ABI');
  return Container.instantiate(module, mounts);
}

function stdout(container) {
  const answer = container.mounts.read(['iso', 'console', 'stdout'].map(s => new TextEncoder().encode(s)));
  check(!answer.error, `stdout read failed: ${answer.error}`);
  return text(answer.ok ?? new Uint8Array());
}

// First transition proof, not yet an Assembly-management implementation. Keeping
// this distinction explicit prevents a host-side demo becoming the architecture.
export async function demo({ compiler, template, baseline, warmup = 10000000, regionMembers, log = () => {}, onArtifact }) {
  const sourceBytes = await optimizerRequest(compiler, 'executable', { wasm: toBase64(template) });
  const source = await executor(sourceBytes, standardMounts());
  const ex = source.instance.exports;
  if (regionMembers !== undefined) {
    check(Number.isInteger(regionMembers) && regionMembers >= 2 && regionMembers <= 32, 'regionMembers must be 2..32');
    check(ex.zaqaru_region_limit?.(regionMembers) === 0, 'region limit unsupported or refused');
  }
  check(ex.zaqaru_run(BigInt(warmup)) === 0, 'workload finished before warmup');
  const warmupRetired = source.value('statistics').retired;
  const referenceSnapshot = source.snapshot();
  const token = 1n;
  const transitionStart = performance.now();
  const traces = ex.zaqaru_freeze(token);
  check(traces > 0, `no hot traces selected: ${traces}`);
  check(ex.zaqaru_run(-1n) === 3, 'frozen source executed');
  check(ex.zaqaru_resume(2n) === -1, 'wrong continuation token accepted');
  const frozen = source.snapshot();
  log(`Frozen ${traces} traces in ${frozen.memory.length} bytes`);
  const started = performance.now();
  const successorBytes = await optimizerRequest(compiler, 'compile', {
    wasm: toBase64(template), memoryLength: frozen.memory.length,
    stackPointer: frozen.stackPointer, pages: sparse(frozen.memory),
  });
  const compileMs = performance.now() - started;
  // Save external state before the validation run consumes the continuation.
  // This mount capture is included in transition time; the artifact-writing
  // hook below runs after all measurements.
  const artifactMounts = onArtifact ? frozen.mounts.save() : null;
  const successor = await executor(successorBytes, frozen.mounts);
  const next = successor.instance.exports;
  check(next.zaqaru_run(-1n) === 3, 'successor did not preserve frozen state');
  check(ex.zaqaru_retire(token) === 0, 'source retirement failed');
  check(ex.zaqaru_resume(token) === -1, 'retired source resumed');
  check(next.zaqaru_resume(token) === 0, 'successor rejected continuation');
  check(next.zaqaru_resume(token) === -1, 'continuation accepted twice');
  const transitionMs = performance.now() - transitionStart;
  log(`Compiled successor in ${compileMs.toFixed(0)} ms; executing`);
  const runStart = performance.now();
  const successorStatus = next.zaqaru_run(-1n);
  const successorMs = performance.now() - runStart;
  const reference = await source.restore(referenceSnapshot);
  const referenceStart = performance.now();
  const referenceStatus = reference.instance.exports.zaqaru_run(-1n);
  const referenceMs = performance.now() - referenceStart;
  check(successorStatus === referenceStatus, `status mismatch: ${successorStatus} / ${referenceStatus}`);
  check((successorStatus & 255) === 2 && (successorStatus >> 8) === 0, `workload failed: ${successorStatus}`);
  check(stdout(successor) === stdout(reference) && stdout(successor).length > 0, 'output mismatch or absent');
  check(successor.value('statistics').retired === reference.value('statistics').retired, 'retired instruction count mismatch');
  check(JSON.stringify(successor.value('processes')) === JSON.stringify(reference.value('processes')), 'final process state mismatch');
  const compiledRetired = next.zaqaru_compiled_retired();
  check(compiledRetired > 0n, 'successor never executed generated code');
  let baselineMs = null;
  let baselineWarmupRetired = null;
  let baselineArtifact;
  if (baseline) {
    const mounts = standardMounts();
    const savedMounts = onArtifact ? mounts.save() : null;
    const historical = await executor(baseline, mounts);
    check(historical.instance.exports.zaqaru_run(BigInt(warmupRetired)) === 0, 'historical baseline ended during warmup');
    baselineWarmupRetired = historical.value('statistics').retired;
    const begin = performance.now();
    check(historical.instance.exports.zaqaru_run(-1n) === successorStatus, 'historical baseline status differs');
    baselineMs = performance.now() - begin;
    check(stdout(historical) === stdout(successor), 'historical baseline output differs');
    check(historical.value('statistics').retired === successor.value('statistics').retired, 'historical retirement differs');
    check(Math.abs(baselineWarmupRetired - warmupRetired) <= 100100,
      `warmup drift exceeds one scheduler quantum: ${warmupRetired} vs ${baselineWarmupRetired}`);
    if (onArtifact) baselineArtifact = { wasm: baseline, mounts: savedMounts,
      expected: { status: successorStatus, stdout: stdout(historical),
        totalRetired: historical.value('statistics').retired,
        processes: historical.value('processes') },
      warmupTarget: warmupRetired, warmupRetired: baselineWarmupRetired };
  }
  const hashes = Object.fromEntries(await Promise.all(
    Object.entries({compiler, template, baseline, successor: successorBytes}).filter(([, bytes]) => bytes)
      .map(async ([name, bytes]) => [name, Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)))
        .map(b => b.toString(16).padStart(2, '0')).join('')])));
  const result = { traces, compileMs, transitionMs, successorMs, referenceMs,
    regionMembers: regionMembers ?? 8,
    hashes,
    executionSpeedup: referenceMs / successorMs,
    includingTransition: referenceMs / (transitionMs + successorMs),
    compiledRetired: compiledRetired.toString(), stdout: stdout(successor),
    regionRetired: (next.zaqaru_region_retired?.() ?? 0n).toString(),
    regionEntries: (next.zaqaru_region_entries?.() ?? 0n).toString(),
    guardWindows: (next.zaqaru_guard_windows?.() ?? 0n).toString(),
    breakEvenRemainingMs: baselineMs !== null && baselineMs > successorMs ? transitionMs / (1 - successorMs / baselineMs) : null,
    successorBytes: successorBytes.length,
    warmupRetired, baselineWarmupRetired, baselineMs, baselineSpeedup: baselineMs === null ? null : baselineMs / successorMs,
    totalRetired: successor.value('statistics').retired,
    normalizedBaselineSpeedup: baselineMs === null ? null :
      baselineMs / successorMs * (successor.value('statistics').retired - warmupRetired) /
      (successor.value('statistics').retired - baselineWarmupRetired),
    baselineIncludingTransition: baselineMs === null ? null : baselineMs / (transitionMs + successorMs),
    engine: globalThis.navigator?.userAgent ?? 'unknown',
    note: 'Single tail measurement after warmup. Historical warmup may differ by one scheduler quantum; normalizedBaselineSpeedup corrects for retired work. executionSpeedup uses an exact paired experimental continuation. Transition includes freeze, optimization and successor instantiation. Not a general or end-to-end speedup claim.' };
  log(JSON.stringify(result));
  if (onArtifact) await onArtifact({
    wasm: successorBytes, mounts: artifactMounts,
    expected: { status: successorStatus, stdout: stdout(reference),
      totalRetired: reference.value('statistics').retired,
      processes: reference.value('processes'),
      compiledRetired: result.compiledRetired, regionRetired: result.regionRetired,
      regionEntries: result.regionEntries },
    measurement: result,
    baselineArtifact,
  });
  return result;
}
