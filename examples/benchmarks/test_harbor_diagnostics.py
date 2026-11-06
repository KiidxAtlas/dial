"""Deterministic diagnostics fixtures; no models, services, credentials or clocks."""
import json
from pathlib import Path
import tempfile
import harbor_diagnostics as diagnostics
import unittest

from harbor_diagnostics import summarize_trial


class HarborDiagnosticsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def put(self, name, value):
        target = self.root / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(value if isinstance(value, str) else json.dumps(value))

    def journal(self, records):
        self.put('agent/decision-journals/session.jsonl', '\n'.join(json.dumps(r) for r in records))

    def test_missing_and_malformed_are_explicit_not_zero_success(self):
        self.put('agent/stdout.jsonl', 'not json\n{}\n')
        result = summarize_trial(self.root)
        kinds = {g['kind'] for g in result['coverage_gaps']}
        self.assertTrue({'malformed_jsonl', 'decision_journal_unavailable', 'per_request_trace_unavailable'} <= kinds)
        self.assertIsNone(result['verifier'])
        self.assertIsNone(result['completion_evidence'])

    def test_unchanged_success_is_distinct_from_refusal_and_private_bodies_stay_private(self):
        records = []
        for index, (error, body) in enumerate([(False, '[Unchanged read] use prior result'), (True, 'This call was not executed. retry barrier')]):
            records.extend([
                {'ts': '2026-11-05T00:00:00Z', 'message': {'role': 'assistant', 'tool_calls': [
                    {'id': str(index), 'function': {'name': 'read', 'arguments': '{"path":"private-secret-value"}'}}]}},
                {'ts': '2026-11-05T00:00:01Z', 'message': {'role': 'tool', 'tool_call_id': str(index),
                    'content': body + ' private-secret-value', 'is_error': error, 'dial_execution_status': 'ok' if not error else 'error'}},
            ])
        self.journal(records)
        self.put('agent/metrics.json', {'usage': {'tool_calls': 2, 'requests': 0}})
        result = summarize_trial(self.root)
        self.assertEqual(result['observed_counts']['tool_errors'], 1)
        self.assertEqual(result['tools'][0]['observations'], ['unchanged_read'])
        self.assertIn('withheld', result['tools'][1]['observations'])
        self.assertEqual(result['tools'][1]['same_arguments_seen_before'], 1)
        self.assertEqual(result['tools'][0]['roundtrip_ms'], 1000)
        self.assertNotIn('private-secret-value', json.dumps(result))
        self.assertIn('sha256', result['source_inventory'][0])

    def test_runtime_and_adaptive_updates_count_one_request(self):
        inference = {'node': {'id': 'r1', 'kind': 'inference', 'result': {'data': {'latency': {'total_ms': 20}}}}}
        def policy(failed):
            return {'message': {'role': 'dial_state', 'patch': {'adaptive_policy': {
                'model': 'claude-sonnet-4-6', 'selection': {'phase': 'editing'},
                'result': {'state': 'response received', 'prompt_tokens': 123,
                           'completion_tokens': 7, 'tool_round_failed': failed, 'finish_reason': 'tool_calls'}}}}}
        self.journal([inference, policy(False), inference, policy(True)])
        result = summarize_trial(self.root)
        self.assertEqual(len(result['requests']), 1)
        request = result['requests'][0]
        self.assertEqual(request['result']['prompt_tokens'], 123)
        self.assertIs(request['result']['tool_round_failed'], True)
        self.assertEqual(request['observed_latency_ms']['total_ms'], 20)

    def test_omp_events_and_official_test_failures_are_recorded_separately(self):
        self.put('agent/stdout.jsonl', '\n'.join(json.dumps(event) for event in [
            {'type': 'message_end', 'message': {'role': 'assistant', 'model': 'claude-sonnet-4-6',
                'usage': {'input': 2, 'cacheRead': 100, 'output': 8}}},
            {'type': 'tool_execution_start', 'toolCallId': 'c1', 'toolName': 'bash', 'args': {'command': 'secret'}},
            {'type': 'tool_execution_end', 'toolCallId': 'c1', 'toolName': 'bash', 'isError': False, 'result': {'text': 'secret'}},
        ]))
        self.put('agent/metrics.json', {'usage': {'requests': 1, 'tool_calls': 1}})
        self.put('verifier/ctrf.json', {'results': {'summary': {'tests': 2, 'passed': 1, 'failed': 1}, 'tests': [
            {'name': 'test_outputs.py::cleanup', 'status': 'failed', 'duration': 0.5, 'message': 'private failure data'}]}})
        result = summarize_trial(self.root)
        self.assertEqual(result['observed_counts'], {'requests': 1, 'tool_results': 1, 'tool_errors': 0})
        self.assertEqual(result['requests'][0]['reported_usage']['cacheRead'], 100)
        self.assertEqual(result['verifier']['summary']['failed'], 1)
        self.assertEqual(result['verifier']['tests'][0]['status'], 'failed')
        self.assertNotIn('private failure data', json.dumps(result))
        self.assertNotIn('secret', json.dumps(result).replace('may contain secrets', ''))

    def test_count_mismatches_are_not_silently_complete(self):
        self.journal([])
        self.put('agent/metrics.json', {'usage': {'requests': 9, 'tool_calls': 4}})
        kinds = {g['kind'] for g in summarize_trial(self.root)['coverage_gaps']}
        self.assertTrue({'request_count_mismatch', 'tool_count_mismatch'} <= kinds)

    def test_provider_retry_metadata_has_source_but_no_error_body(self):
        self.put('agent/stderr.txt', 'transient provider failure: secret endpoint; retrying in 2.3s\n')
        result = summarize_trial(self.root)
        self.assertEqual(result['stderr_events'][0]['observations'], ['provider_retry'])
        self.assertEqual(result['stderr_events'][0]['source']['line'], 1)
        self.assertNotIn('secret endpoint', json.dumps(result))

    def test_unchecked_metadata_values_do_not_export_private_bodies(self):
        self.journal([{'message': {'role': 'tool', 'tool_call_id': 'x', 'content': '', 'is_error': False,
            'dial_execution': {'output': {'capture': {'output': 'private body'}}}}}])
        self.put('verifier/ctrf.json', {'results': {'tests': [{'duration': 'private body'}]}})
        self.assertNotIn('private body', json.dumps(summarize_trial(self.root)))

    def test_schema_malformed_records_become_gaps(self):
        for malformed in ({'message': ['invalid']}, {'node': 'invalid'},
                          {'message': {'patch': {'adaptive_policy': ['invalid']}}}):
            with self.subTest(record=malformed):
                self.journal([malformed])
                self.put('agent/metrics.json', [])
                result = summarize_trial(self.root)
                self.assertIn('malformed_jsonl', {g['kind'] for g in result['coverage_gaps']})

    def test_missing_error_flag_is_unknown_not_zero(self):
        self.put('agent/stdout.jsonl', json.dumps({'type': 'tool_execution_end',
            'toolCallId': 'x', 'toolName': 'bash', 'result': {'text': 'done'}}))
        result = summarize_trial(self.root)
        self.assertIsNone(result['observed_counts']['tool_errors'])
        self.assertIn('tool_error_classification_missing', {g['kind'] for g in result['coverage_gaps']})

    def test_stage_timings_do_not_blend_setup_with_agent_execution(self):
        self.put('result.json', {'agent_setup': {'started_at': '2026-11-05T00:00:00Z',
                                               'finished_at': '2026-11-05T00:00:02Z'},
                                 'agent_execution': {'started_at': '2026-11-05T00:00:03Z',
                                                    'finished_at': '2026-11-05T00:00:08Z'}})
        timing = summarize_trial(self.root)['stage_durations_ms']
        self.assertEqual(timing['agent_setup_ms'], 2000)
        self.assertEqual(timing['agent_execution_ms'], 5000)
        self.assertIsNone(timing['verifier_ms'])

    def test_symlink_outside_trial_is_not_read(self):
        with tempfile.TemporaryDirectory() as outside:
            private = Path(outside) / 'private.jsonl'
            private.write_text('private body')
            target = self.root / 'agent/stdout.jsonl'
            target.parent.mkdir(parents=True)
            target.symlink_to(private)
            result = summarize_trial(self.root)
            self.assertIn('outside_trial_symlink', {g['kind'] for g in result['coverage_gaps']})
            self.assertFalse(any(s['path'] == 'agent/stdout.jsonl' for s in result['source_inventory']))

    def test_bad_verifier_list_and_aggregate_values_do_not_abort_or_leak(self):
        self.put('verifier/ctrf.json', {'results': {'tests': None}})
        self.put('agent/metrics.json', {'usage': {'tool_calls': {'private body': 1}}})
        result = summarize_trial(self.root)
        self.assertIn('malformed_json', {g['kind'] for g in result['coverage_gaps']})
        self.assertNotIn('private body', json.dumps(result))

    def test_runtime_checks_reconcile_without_inventing_model_calls(self):
        self.journal([
            {'message': {'role': 'assistant', 'tool_calls': [
                {'id': 'model-call', 'function': {'name': 'read', 'arguments': '{}'}}]}},
            {'message': {'role': 'tool', 'tool_call_id': 'model-call', 'content': 'private body', 'is_error': False}},
        ])
        self.put('agent/stdout.jsonl', json.dumps({'type': 'result', 'tool_calls': [
            {'call_id': 'model-call', 'name': 'read', 'is_error': False, 'seconds': 0.1},
            {'call_id': 'focused-check:private-check', 'name': 'bash', 'is_error': True,
             'seconds': 0.2, 'summary': 'private command'},
            {'call_id': 'focused-check:private-check', 'name': 'bash', 'is_error': False, 'seconds': 0.3},
        ]}))
        self.put('agent/metrics.json', {'usage': {'tool_calls': 3, 'tool_errors': 1, 'requests': 0}})
        result = summarize_trial(self.root)
        self.assertEqual(result['observed_counts']['tool_results'], 3)
        self.assertEqual(result['observed_counts']['tool_errors'], 1)
        checks = [t for t in result['tools'] if t.get('origin') == 'runtime_focused_check']
        self.assertEqual(len(checks), 2)
        self.assertEqual(checks[0]['execution_duration_ms'], 200)
        self.assertEqual(checks[0]['ledger_source']['json_pointer'], '/tool_calls/1')
        self.assertIsNone(checks[0]['result_source'])
        self.assertNotIn('tool_count_mismatch', {g['kind'] for g in result['coverage_gaps']})
        self.assertNotIn('private', json.dumps(result).replace(result['privacy'], ''))

    def test_only_final_result_ledger_is_reconciled(self):
        self.journal([])
        event = {'type': 'result', 'tool_calls': [
            {'call_id': 'focused-check:check', 'name': 'bash', 'is_error': False, 'seconds': 0.1}]}
        self.put('agent/stdout.jsonl', json.dumps(event) + '\n' + json.dumps(event))
        self.put('agent/metrics.json', {'usage': {'tool_calls': 1}})
        result = summarize_trial(self.root)
        self.assertEqual(result['observed_counts']['tool_results'], 1)
        self.assertEqual(result['tools'][0]['ledger_source']['line'], 2)

    def test_trial_contract_efficiency_and_verifier_retries_are_explicit(self):
        self.put('result.json', {'task_checksum': 'a' * 64,
            'agent_info': {'version': 'dial-patched', 'model_info': {'name': 'sonnet', 'provider': 'anthropic'}},
            'agent_result': {'cost_usd': None}, 'config': {
                'agent': {'model_name': 'anthropic/sonnet', 'override_timeout_sec': 60, 'model_api_key_name': 'private body'},
                'environment': {'type': 'docker', 'override_cpus': 2, 'kwargs': {'secret': 'private body'}},
                'verifier': {'disable': False}}})
        self.put('agent/metrics.json', {'agent': 'wrong-agent', 'exit_code': 1, 'usage': {'efficiency': {
            'inference_latency': {'failed_requests': 2, 'retry_backoffs': 3, 'retry_backoff_ms': 40},
            'projection_bytes': {'history_bytes': 100, 'requests_by_phase': {'recovery': 2}, 'private': 'private body'},
            'storage_io': {'runtime_flush_micros': 11}}}})
        self.put('verifier/ctrf.json', {'results': {'tests': [{'name': 'cleanup', 'status': 'failed', 'duration': 1, 'retries': 4}]}})
        result = summarize_trial(self.root)
        metadata = result['trial_metadata']
        self.assertEqual(metadata['task_checksum'], 'a' * 64)
        self.assertEqual(metadata['reported_agent'], 'dial-patched')
        self.assertEqual(metadata['constraints']['environment']['override_cpus'], 2)
        self.assertEqual(metadata['exit_code'], 1)
        self.assertIsNone(metadata['reported_cost_usd'])
        self.assertIn('agent_identity_mismatch', {g['kind'] for g in result['coverage_gaps']})
        self.assertEqual(result['reported_efficiency']['inference_latency']['retry_backoffs'], 3)
        self.assertEqual(result['reported_efficiency']['projection_bytes']['requests_by_phase']['recovery'], 2)
        self.assertEqual(result['verifier']['tests'][0]['retries'], 4)
        self.assertNotIn('private body', json.dumps(result))

    def test_overview_keeps_partial_measurements_and_empty_agents_unknown(self):
        rows = [{'batch': 'threeway', 'task': 'task', 'agent': 'a', 'job': str(i),
                 'excluded_setup_failure': False, 'reward': {'reward': 1}, 'exception': None,
                 'elapsed_sec': elapsed, 'usage': {'requests': 1, 'tool_calls': 0},
                 'diagnostics': {'requests': [{'observed_latency_ms': {'total_ms': 10}}] if i == 0 else [],
                                 'tools': [], 'coverage_gaps': []}}
                for i, elapsed in enumerate((2, None))]
        overview = diagnostics.report_overview(rows, ['task'], ('a', 'b'))
        batch = overview['batches']['threeway']
        self.assertEqual(batch['missing_pairs'], [{'task': 'task', 'agent': 'b'}])
        self.assertEqual(batch['duplicate_pairs'], [{'task': 'task', 'agent': 'a', 'trials': 2}])
        elapsed = batch['agents']['a']['measurements']['agent_elapsed_sec']
        self.assertEqual(elapsed['samples'], 1)
        self.assertEqual(elapsed['observed_sum'], 2)
        self.assertIsNone(elapsed['total'])
        latency = batch['agents']['a']['request_latency_ms']
        self.assertEqual(latency['samples'], 1)
        self.assertEqual(latency['expected_samples'], 2)
        self.assertFalse(latency['complete'])
        self.assertIsNone(batch['agents']['b']['measurements']['agent_elapsed_sec']['total'])

    def test_overview_reports_incomplete_jobs_without_discarding_them(self):
        self.put('jobs/threeway-pending/result.json', {'n_total_trials': 1,
            'stats': {'n_completed_trials': 1, 'n_retries': 2, 'n_errored_trials': 0}})
        self.put('jobs/serial-other/result.json', {'n_total_trials': 99})
        result = diagnostics.report_overview([], ['task'], ('a',), root=self.root, requested_batch='threeway')
        self.assertEqual(len(result['jobs']), 1)
        job = result['jobs'][0]
        self.assertEqual(job['counts']['n_retries'], 2)
        self.assertEqual(job['discovered_trial_results'], 0)
        self.assertIn('job_trial_result_count_mismatch', {g['kind'] for g in job['coverage_gaps']})
        self.assertEqual(result['batches']['threeway']['missing_pairs'], [{'task': 'task', 'agent': 'a'}])

    def test_overview_surfaces_failed_tests_and_separate_retry_error_layers(self):
        row = {'batch': 'threeway', 'task': 'task', 'agent': 'a', 'job': 'threeway-task-a',
               'excluded_setup_failure': False, 'reward': {'reward': 0}, 'exception': None,
               'usage': {'requests': 1, 'tool_calls': 1, 'tool_errors': 1},
               'diagnostics': {'requests': [], 'tools': [{'tool': 'bash', 'is_error': True,
                    'same_arguments_seen_before': 1, 'observations': ['permission'], 'roundtrip_ms': None}],
                    'reported_efficiency': {'inference_latency': {'retry_backoffs': 2}},
                    'trial_metadata': {'exit_code': 0},
                    'verifier': {'tests': [{'name': 'cleanup', 'status': 'failed', 'retries': 3}]},
                    'coverage_gaps': [{'kind': 'request_latency_unavailable'}]}}
        batch = diagnostics.report_overview([row], ['task'], ('a',))['batches']['threeway']
        trial = batch['tasks']['task']['a'][0]
        self.assertEqual(trial['failed_tests'], ['cleanup'])
        self.assertEqual(trial['exit_code'], 0)
        self.assertEqual(trial['verifier_retries'], 3)
        agent = batch['agents']['a']
        self.assertEqual(agent['tools']['bash']['known_errors'], 1)
        self.assertEqual(agent['tools']['bash']['repeated_arguments'], 1)
        self.assertEqual(agent['tools']['bash']['observations'], {'permission': 1})
        self.assertEqual(agent['measurements']['inference_latency.retry_backoffs']['total'], 2)
        self.assertEqual(agent['coverage_gaps'], {'request_latency_unavailable': 1})

    def test_task_checksums_flag_incompatible_or_unknown_inputs(self):
        rows = [{'batch': 'threeway', 'task': 'task', 'agent': agent, 'job': agent,
                 'reward': {'reward': 1}, 'diagnostics': {'trial_metadata': {'task_checksum': checksum}}}
                for agent, checksum in (('a', 'a' * 64), ('b', 'b' * 64))]
        result = diagnostics.report_overview(rows, ['task'], ('a', 'b'))
        check = result['batches']['threeway']['task_contracts']['task']
        self.assertEqual(check['status'], 'mismatch')
        rows[1]['diagnostics']['trial_metadata']['task_checksum'] = None
        check = diagnostics.report_overview(rows, ['task'], ('a', 'b'))['batches']['threeway']['task_contracts']['task']
        self.assertEqual(check['status'], 'unknown')
        self.assertEqual(check['observed_checksums'], ['a' * 64])

    def test_job_metadata_is_bounded_and_confined_and_malformed_jobs_stay_visible(self):
        self.put('jobs/threeway-malformed/result.json', {'stats': []})
        with tempfile.TemporaryDirectory() as temporary:
            outside = Path(temporary) / 'result.json'
            outside.write_text('{"n_total_trials":123}')
            target = self.root / 'jobs/threeway-outside/result.json'
            target.parent.mkdir(parents=True)
            target.symlink_to(outside)
            jobs = diagnostics.report_overview([], [], (), root=self.root, requested_batch='threeway')['jobs']
        self.assertEqual(len(jobs), 2)
        by_name = {j['job']: j for j in jobs}
        self.assertIn('malformed_json', {g['kind'] for g in by_name['threeway-malformed']['coverage_gaps']})
        self.assertIn('outside_trial_symlink', {g['kind'] for g in by_name['threeway-outside']['coverage_gaps']})
        self.assertIsNone(by_name['threeway-outside']['counts']['n_total_trials'])

    def test_markdown_surfaces_coverage_and_missing_measurements(self):
        overview = diagnostics.report_overview([], ['task'], ('agent',), requested_batch='threeway')
        text = diagnostics.render_overview(overview)
        self.assertIn('threeway', text)
        self.assertIn('Missing: task / agent', text)
        self.assertIn('agent_elapsed_sec', text)
        self.assertIn('unknown', text)
        self.assertIn('not causal estimates', text)
        rows = [{'batch': 'threeway', 'task': 'task', 'agent': 'agent', 'job': str(i),
                 'reward': {'reward': 1}} for i in range(2)]
        text = diagnostics.render_overview(diagnostics.report_overview(rows, ['task'], ('agent',)))
        table = text.split('### Task outcomes', 1)[1].split('### Identity', 1)[0]
        self.assertEqual(len([line for line in table.splitlines() if line.startswith('|')]), 4)

    def test_request_coverage_cannot_reconcile_mismatches_across_trials(self):
        rows = [{'usage': {'requests': 1}, 'diagnostics': {'requests': requests}}
                for requests in ([{'observed_latency_ms': {'total_ms': 1}}] * 2, [])]
        latency = diagnostics.summarize_agent(rows)['request_latency_ms']
        self.assertEqual(latency['samples'], 2)
        self.assertEqual(latency['expected_samples'], 2)
        self.assertFalse(latency['complete'])
        self.assertIsNone(latency['total'])


if __name__ == '__main__':
    unittest.main()
