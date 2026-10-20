#!/usr/bin/env python3
"""Executable negative/positive fixtures; NOT provider measurements."""
import copy
import importlib.util
import pathlib
import unittest

path = pathlib.Path(__file__).with_name('triad_paired_gate.py')
spec = importlib.util.spec_from_file_location('triad_paired_gate', path)
triad = importlib.util.module_from_spec(spec)
spec.loader.exec_module(triad)


def fixtures(n=24):
    prices = {'version':'fixture-v1', 'source':'unit fixture only', 'models':{
        'fixture-model': {'uncached_input_nano_usd':100,
                         'cached_input_nano_usd':10,
                         'cache_creation_input_nano_usd':125,
                         'output_nano_usd':200}}}
    rows=[]
    for task in range(n):
        for arm in ('baseline','candidate'):
            rows.append(dict(task_id=f'task-{task:03}', repetition=0, arm=arm,
                resolved=True, reported_model='fixture-model', usage_basis='provider_reported',
                input_tokens=1000, cached_input_tokens=0 if arm=='baseline' else 600,
                cache_write_tokens=0, output_tokens=10,
                prompt_bytes=4096 if arm=='baseline' else 2800,
                tool_schema_bytes=1024 if arm=='baseline' else 256,
                compacted_evidence_bytes=0 if arm=='baseline' else 256,
                wall_ms=1500 if arm=='baseline' else 1100))
    return {'experiment_id':'fixture-not-provider','rows':rows}, prices


class GateFixtures(unittest.TestCase):
    def test_all_good_noninferior_and_cheaper(self):
        report, prices = fixtures()
        result=triad.analyze(report,prices,resamples=200)
        self.assertTrue(result['cost_saving_claim_allowed'])
        self.assertEqual(result['cache_read_token_delta_mean'],600)

    def test_unknown_write_split_blocks_cost_claim(self):
        report, prices = fixtures()
        report['rows'][0]['cache_write_tokens'] = None
        result=triad.analyze(report, prices,resamples=200)
        self.assertFalse(result['cost_saving_claim_allowed'])
        self.assertIsNone(result['estimated_cost_delta_nano_usd'])

    def test_unknown_cache_read_split_blocks_cost_claim(self):
        report, prices = fixtures()
        report['rows'][0]['cached_input_tokens'] = None
        result=triad.analyze(report,prices,resamples=200)
        self.assertFalse(result['cost_saving_claim_allowed'])

    def test_quality_regression_blocks_savings(self):
        report, prices = fixtures()
        for row in report['rows']:
            if row['arm']=='candidate': row['resolved']=False
        result=triad.analyze(report,prices,resamples=200)
        self.assertEqual(result['quality_gate'],'regressed')
        self.assertFalse(result['cost_saving_claim_allowed'])

    def test_small_sample_cannot_promote(self):
        report, prices = fixtures(2)
        result=triad.analyze(report,prices,resamples=200)
        self.assertEqual(result['quality_gate'],'insufficient_tasks')
        self.assertFalse(result['cost_saving_claim_allowed'])

    def test_missing_pair_is_refused(self):
        report, prices = fixtures()
        report['rows'].pop()
        with self.assertRaisesRegex(triad.GateError,'unpaired'):
            triad.analyze(report,prices,resamples=200)

    def test_duplicates_are_refused(self):
        report, prices = fixtures()
        report['rows'].append(copy.deepcopy(report['rows'][0]))
        with self.assertRaisesRegex(triad.GateError,'duplicate'):
            triad.analyze(report,prices,resamples=200)

    def test_different_models_are_refused(self):
        report, prices = fixtures()
        report['rows'][0]['reported_model']='other-model'
        with self.assertRaisesRegex(triad.GateError,'different provider models'):
            triad.analyze(report,prices,resamples=200)

    def test_usage_invariant_is_refused(self):
        report, prices = fixtures()
        report['rows'][0]['cache_write_tokens']=1001
        with self.assertRaisesRegex(triad.GateError,'exceeds input_tokens'):
            triad.analyze(report,prices,resamples=200)

    def test_synthetic_usage_never_priced(self):
        report, prices = fixtures()
        report['rows'][0]['usage_basis']='estimated'
        result=triad.analyze(report,prices,resamples=200)
        self.assertFalse(result['cost_saving_claim_allowed'])

    def test_estimated_cost_increase_not_claimed(self):
        report, prices = fixtures()
        for row in report['rows']:
            if row['arm']=='candidate':
                row['cached_input_tokens']=0
                row['input_tokens']=2000
        result=triad.analyze(report,prices,resamples=200)
        self.assertFalse(result['cost_saving_claim_allowed'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
