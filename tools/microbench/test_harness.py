"""Harness correctness checks; no benchmark engine or Linux guest required."""
import copy
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
