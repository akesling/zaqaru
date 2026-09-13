"""Harness correctness checks; no benchmark engine or Linux guest required."""
import copy
import json
import tempfile
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import compare
import latency
import measure


class HarnessTests(unittest.TestCase):
    def test_parse_requires_checksum_and_retirement(self):
        with self.assertRaises(ValueError):
            measure.parse_sample('', '', 'alu', 1, 'native')
        with self.assertRaises(ValueError):
            measure.parse_sample('alu 1', '', 'alu', 1, 'bytecode')
        row = measure.parse_sample('alu 1', '123 instructions in 1.0s', 'alu', 1, 'bytecode')
        self.assertEqual(row['retired'], 123)

    def test_reject_noisy_or_inconsistent_counts(self):
        with self.assertRaises(ValueError):
            measure.summarize([{'total': 2}], [{'total': 1}], 1)
        with self.assertRaises(ValueError):
            measure.summarize([{'total': 1, 'retired': 1}, {'total': 1, 'retired': 2}],
                              [{'total': 2, 'retired': 3}], 1)

    def test_first_request_is_measured(self):
        with patch.object(latency, 'fetch', return_value=(200, .01, 10)) as fetch, \
             patch.object(latency, 'metadata', return_value={}):
            result = latency.measure(80, 'test', 0)
        self.assertEqual(fetch.call_count, 1 + latency.WARMUP + latency.SEQUENTIAL + latency.CONCURRENCY * latency.EACH)
        self.assertEqual(result['cold_ms'], 10)

    def test_http_failures_in_every_phase(self):
        for offset in (0, 1, 1 + latency.WARMUP, 1 + latency.WARMUP + latency.SEQUENTIAL):
            calls = [(200, .01, 10)] * offset + [(500, .01, 10)]
            with self.subTest(offset=offset), patch.object(latency, 'fetch', side_effect=calls + [(200, .01, 10)] * 100):
                with self.assertRaises(ValueError):
                    latency.measure(80, 'test', 0)

    def test_ab_uses_distinct_engines_and_matching_workloads(self):
        clock = [0.0]
        modules = {}
        executions = []
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            current, baseline = root / 'current', root / 'baseline'
            current.write_bytes(b'current')
            baseline.write_bytes(b'baseline')
            output = root / 'results.json'

            def run(command, **kwargs):
                if command[0] == 'gcc':
                    return SimpleNamespace(stdout='', stderr='')
                if command[1] == 'bake':
                    executable = command[0]
                    path = command[command.index('-o') + 1]
                    modules[path] = (executable, command[-2], int(command[-1]))
                    return SimpleNamespace(stdout='', stderr='')
                self.assertEqual(command[:3], ['taskset', '-c', '0'])
                executable = command[3]
                if command[4] == 'run':
                    baked_by, name, scale = modules[command[5]]
                    self.assertEqual(executable, baked_by)
                    executions.append(executable)
                    clock[0] += 1 + scale * (3 if executable == str(baseline) else 2)
                    return SimpleNamespace(stdout=f'{name} {scale}',
                                           stderr=f'{scale * 100} instructions in 1.0s')
                name, scale = command[-2], int(command[-1])
                clock[0] += 1
                return SimpleNamespace(stdout=f'{name} {scale}', stderr='')

            argv = ['measure', '--binary', str(current), '--against', str(baseline),
                    '--modes', 'bytecode', '--kernels', 'mixed', '--repeats', '2',
                    '--output', str(output)]
            with patch('sys.argv', argv), patch.object(measure, 'metadata', return_value={}), \
                 patch.object(measure.platform, 'system', return_value='Linux'), \
                 patch.object(measure.platform, 'machine', return_value='x86_64'), \
                 patch.object(measure.os, 'sched_getaffinity', return_value={0}, create=True), \
                 patch.object(measure.subprocess, 'run', side_effect=run), \
                 patch.object(measure.subprocess, 'check_output', return_value='compiler'), \
                 patch.object(measure.time, 'perf_counter', side_effect=lambda: clock[0]):
                measure.main()
            result = json.loads(output.read_text())
            self.assertEqual(result['speedup_geomean'], 1.5)
            self.assertEqual(set(result['kernels']['mixed']), {'bytecode', 'baseline'})
            # Four fixed samples, then current/baseline at each scale; the
            # next repetition reverses their order to balance drift.
            self.assertEqual(executions[4:], [str(p) for p in
                             (current, baseline, current, baseline,
                              baseline, current, baseline, current)])

    def test_baseline_regression_and_environment(self):
        meta = {k: 'same' for k in ('platform', 'machine', 'cpu', 'environment', 'core', 'compiler')}
        row = {'scale': 1, 'retired': 10, 'per_unit': 1}
        before = {'schema': 1, 'metadata': meta, 'kernels': {'alu': {'interpreter': row, 'bytecode': row}}}
        after = copy.deepcopy(before)
        after['kernels']['alu']['bytecode'] = dict(row, per_unit=1.2)
        self.assertEqual(compare.compare(before, after, .1), ['alu/bytecode'])
        after['metadata']['machine'] = 'different'
        with self.assertRaises(ValueError):
            compare.compare(before, after, .1)


if __name__ == '__main__':
    unittest.main()
