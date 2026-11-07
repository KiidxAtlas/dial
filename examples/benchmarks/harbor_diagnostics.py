"""Private, offline diagnostics for the Harbor pilot; never changes scores or agent inputs.

Only metadata is exported: source hashes/line references, numeric counters and bounded labels.
Raw prompts, arguments, output bodies, credentials and provider reasoning stay in private logs.
Pattern labels are observations, not causal findings. Missing measurements remain unknown.
"""
from collections import Counter
from datetime import datetime
import hashlib
import json
import math
from pathlib import Path
import re
from cross_agent import median, percentile95

MAX_BYTES = 32 * 1024 * 1024
MAX_JOURNALS = 100
PATTERNS = {
    'unchanged_read': ('[unchanged read]',),
    'withheld': ('was not executed', 'retry barrier', 'now withheld', 'call withheld'),
    'permission': ('permission denied', 'requires directory approval', 'approval required'),
    'missing_artifact': ('artifact', 'does not exist'),
    'pty_disabled': ('pty is disabled',),
    'missing_dependency': ('command not found', 'no such file or directory'),
    'timeout': ('timed out', 'deadline exceeded'),
    'provider_retry': ('transient provider failure', 'retrying in'),
}

USAGE_COUNTERS = ('input_total', 'uncached_input', 'cache_read', 'cache_write', 'output',
                  'requests', 'tool_calls', 'tool_errors')
EFFICIENCY_FIELDS = {
    'inference_latency': ('requests', 'failed_requests', 'retry_backoffs', 'retry_backoff_ms',
                          'total_ms', 'response_wait_ms', 'generation_ms', 'loading_ms',
                          'prompt_processing_ms', 'cancellation_ms'),
    'storage_io': ('context_sync_attempts', 'context_sync_failures', 'context_sync_micros',
                   'control_publication_attempts', 'control_publication_failures', 'control_publication_micros',
                   'equal_control_publications', 'equal_control_publication_micros', 'runtime_flush_attempts',
                   'runtime_flush_failures', 'runtime_flush_micros', 'runtime_reclaim_sync_attempts',
                   'runtime_reclaim_sync_failures', 'runtime_reclaim_sync_micros'),
    'projection_bytes': ('requests', 'system_bytes', 'tool_schema_bytes', 'history_bytes',
                         'stable_message_bytes', 'changed_message_bytes', 'request_tail_bytes',
                         'full_result_bytes', 'visible_result_bytes', 'reduced_results', 'failed_tool_results',
                         'observed_reply_text_bytes', 'observed_tool_argument_bytes', 'lightweight_requests',
                         'first_tool_result_ms', 'first_workspace_change_result_ms',
                         'requests_before_first_workspace_change_result', 'delivered_worker_results',
                         'parent_inspections_matching_delivered_workers'),
}
PHASES = ('general', 'investigation', 'planning', 'editing', 'recovery', 'verification', 'completion', 'unknown')


def read_source(path, root, inventory, gaps, required=False):
    """Bounded, confined source reader shared by trial and job metadata."""
    where = {'path': path.relative_to(root).as_posix()}
    try:
        if not path.resolve().is_relative_to(root.resolve()):
            gaps.append({'kind': 'outside_trial_symlink', **where})
            return None
        if not path.is_file():
            if required:
                gaps.append({'kind': 'missing_file', **where})
            return None
        if path.stat().st_size > MAX_BYTES:
            gaps.append({'kind': 'oversized_file', **where, 'limit_bytes': MAX_BYTES})
            return None
        data = path.read_bytes()
    except OSError:
        gaps.append({'kind': 'unreadable_file', **where})
        return None
    inventory.append({**where, 'bytes': len(data), 'sha256': digest(data)})
    return data


def efficiency_metadata(usage):
    efficiency = usage.get('efficiency') or {}
    if not isinstance(efficiency, dict):
        return {}
    result = {'duration_ms': number(efficiency.get('duration_ms'))}
    for group, fields in EFFICIENCY_FIELDS.items():
        values = efficiency.get(group)
        if isinstance(values, dict):
            result[group] = {k: number(values[k]) for k in fields if k in values}
    projection = efficiency.get('projection_bytes') or {}
    if isinstance(projection, dict):
        phases = projection.get('requests_by_phase')
        if isinstance(phases, dict):
            result.setdefault('projection_bytes', {})['requests_by_phase'] = {
                k: number(phases[k]) for k in PHASES if k in phases}
    return result


def trial_metadata(result, metrics):
    def obj(value):
        return value if isinstance(value, dict) else {}

    config, info = obj(result.get('config')), obj(result.get('agent_info'))
    model, agent = obj(info.get('model_info')), obj(config.get('agent'))
    checksum = result.get('task_checksum')
    constraints = {}
    for group, fields in {
        'agent': ('override_timeout_sec', 'override_setup_timeout_sec', 'max_timeout_sec'),
        'environment': ('override_cpus', 'override_gpus', 'override_memory_mb', 'override_storage_mb'),
        'verifier': ('override_timeout_sec', 'max_timeout_sec'),
    }.items():
        values = obj(config.get(group))
        constraints[group] = {k: number(values.get(k)) for k in fields}
    environment = obj(config.get('environment'))
    constraints['environment']['type'] = label(environment['type']) if environment.get('type') is not None else None
    disabled = obj(config.get('verifier')).get('disable')
    constraints['verifier']['disabled'] = disabled if isinstance(disabled, bool) else None
    return {'source': {'path': 'result.json'},
            'task_checksum': checksum if isinstance(checksum, str) and re.fullmatch('[0-9a-f]{64}', checksum) else None,
            'reported_agent': label(info['version']) if info.get('version') is not None else None,
            'metric_agent': label(metrics['agent']) if metrics.get('agent') is not None else None,
            'configured_model': label(agent['model_name']) if agent.get('model_name') is not None else None,
            'reported_model': label(model['name']) if model.get('name') is not None else None,
            'reported_provider': label(model['provider']) if model.get('provider') is not None else None,
            'exit_code': number(metrics.get('exit_code')),
            'reported_cost_usd': number(obj(result.get('agent_result')).get('cost_usd')),
            'constraints': constraints}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def label(value):
    return value if isinstance(value, str) and re.fullmatch(r'[A-Za-z0-9_./:+-]{1,120}', value) else 'unknown'


def number(value):
    return value if isinstance(value, (int, float)) and (not isinstance(value, float) or math.isfinite(value)) else None


def numbers(mapping):
    return {k: v for k, v in mapping.items() if number(v) is not None} if isinstance(mapping, dict) else {}


def controls_metadata(controls):
    """Allowlisted native request controls, excluding arbitrary strings and request content."""
    out = {'model': label(controls.get('model'))}
    out.update({key: number(controls[key]) for key in ('max_tokens', 'temperature', 'top_p')
                if number(controls.get(key)) is not None})
    thinking = controls.get('thinking')
    if isinstance(thinking, dict):
        out['thinking'] = {}
        if thinking.get('type') in ('disabled', 'enabled', 'adaptive', 'between_tools'):
            out['thinking']['type'] = thinking['type']
        if thinking.get('display') in ('summarized', 'omitted'):
            out['thinking']['display'] = thinking['display']
        if number(thinking.get('budget_tokens')) is not None:
            out['thinking']['budget_tokens'] = number(thinking['budget_tokens'])
    output = controls.get('output_config')
    if isinstance(output, dict) and output.get('effort') in ('low', 'medium', 'high', 'xhigh', 'max'):
        out['output_config'] = {'effort': output['effort']}
    return out


def validate_event(event):
    paths = ['node', 'node.result', 'node.result.data', 'node.result.data.latency', 'message',
             'message.usage', 'message.dial_execution', 'message.dial_execution.output',
             'message.dial_execution.process', 'message.dial_evidence']
    for state in ('message.patch', 'message.state'):
        paths.extend([state, state + '.adaptive_policy', state + '.adaptive_policy.result',
                      state + '.adaptive_policy.selection', state + '.reliability',
                      state + '.reliability.stats', state + '.completion_evidence'])
    for path in paths:
        value = event
        for key in path.split('.'):
            value = value.get(key) if isinstance(value, dict) else None
        if value is not None and not isinstance(value, dict):
            raise ValueError('Invalid object shape')
    node = event.get('node') or {}
    if node.get('kind') == 'inference' and not isinstance(node.get('id'), str):
        raise ValueError('Missing inference identity')
    message = event.get('message') or {}
    calls = message.get('tool_calls') or []
    if not isinstance(calls, list) or any(not isinstance(c, dict) or not isinstance(c.get('function'), dict) for c in calls):
        raise ValueError('Invalid tool calls')
    if any(not isinstance(c.get('id'), str) for c in calls):
        raise ValueError('Missing tool identity')
    if message.get('role') == 'tool' and not isinstance(message.get('tool_call_id'), str):
        raise ValueError('Missing tool result identity')
    for value in (message.get('tool_call_id'), event.get('toolCallId')):
        if value is not None and not isinstance(value, str):
            raise ValueError('Invalid tool identity')
    changed = (message.get('dial_execution') or {}).get('changed_paths')
    if changed is not None and not isinstance(changed, list):
        raise ValueError('Invalid changed paths')
    for state in ('patch', 'state'):
        reliability = ((message.get(state) or {}).get('reliability') or {})
        offered = reliability.get('current_tools')
        if offered is not None and not isinstance(offered, list):
            raise ValueError('Invalid capability list')


def capture_metadata(output):
    enums = {'unknown', 'complete', 'partial', 'none', 'expired', 'unavailable', 'omitted', 'truncated'}
    result = {k: v if isinstance(v, str) and v in enums else 'unknown'
              for k, v in output.items() if k in ('capture', 'retention', 'presentation')}
    result.update({k: number(v) for k, v in output.items()
                   if k in ('omitted_bytes', 'original_bytes', 'presented_bytes', 'retained_bytes')})
    return result


def labels(text):
    text = str(text).lower()
    return [name for name, terms in PATTERNS.items()
            if (all(term in text for term in terms) if name == 'missing_artifact'
                else any(term in text for term in terms))]


def milliseconds(start, end):
    try:
        delta = (datetime.fromisoformat(end.replace('Z', '+00:00')) -
                 datetime.fromisoformat(start.replace('Z', '+00:00'))).total_seconds() * 1000
        return round(delta, 3) if delta >= 0 else None
    except (ValueError, TypeError, AttributeError):
        return None


def summarize_trial(trial):
    """Read only this trial's allowlisted logs; return metadata plus explicit coverage gaps."""
    trial = Path(trial)
    gaps, inventory, requests, tools, stderr_events = [], [], {}, {}, []
    adaptive, completion, runtime_latency = {}, None, {}
    seen_calls = Counter()

    def source(path, line=None):
        return {'path': path.relative_to(trial).as_posix(), **({'line': line} if line else {})}

    def read(path, required=False):
        return read_source(path, trial, inventory, gaps, required)

    def records(path):
        data = read(path, required=True)
        if data is None:
            return
        for line, raw in enumerate(data.splitlines(), 1):
            try:
                event = json.loads(raw)
                if not isinstance(event, dict):
                    raise ValueError('not an object')
                validate_event(event)
            except (ValueError, UnicodeDecodeError):
                gaps.append({'kind': 'malformed_jsonl', **source(path, line)})
                continue
            yield line, event

    def tool_start(key, name, arguments, where, timestamp=None):
        timestamp = timestamp if milliseconds(timestamp, timestamp) == 0 else None
        encoded = json.dumps(arguments, sort_keys=True, ensure_ascii=True).encode()
        identity = (label(name), digest(encoded))
        seen_calls[identity] += 1
        tools[key] = {'tool': label(name), 'origin': 'model_tool', 'call_source': where, 'call_timestamp': timestamp,
                      'argument_sha256': identity[1], 'argument_bytes': len(encoded),
                      'same_arguments_seen_before': seen_calls[identity] - 1,
                      'result_source': None, 'is_error': None, 'roundtrip_ms': None}

    def tool_end(key, text, message, where, timestamp=None, omp=False):
        record = tools.setdefault(key, {'tool': 'unknown', 'call_source': None, 'call_timestamp': None})
        execution = message.get('dial_execution') or {}
        record.update(result_source=where, result_sha256=digest(text.encode()), result_bytes=len(text.encode()),
                      visible_result_bytes=len(str(message.get('dial_model_content', text)).encode()),
                      is_error=(error if isinstance(error := (message.get('isError') if omp else message.get('is_error', (message.get('dial_evidence') or {}).get('is_error'))), bool) else None),
                      execution_status=label(message.get('dial_execution_status')),
                      roundtrip_ms=milliseconds(record.get('call_timestamp'), timestamp),
                      observations=labels(text), process=numbers(execution.get('process')),
                      capture=capture_metadata(execution.get('output') or {}),
                      changed_path_count=len(execution.get('changed_paths') or []))

    journals = sorted((trial / 'agent/decision-journals').glob('*.jsonl'))
    if len(journals) > MAX_JOURNALS:
        gaps.append({'kind': 'journal_limit', 'found': len(journals), 'limit': MAX_JOURNALS})
    for path in journals[:MAX_JOURNALS]:
        active_request = None
        active_controls = None
        for line, event in records(path):
            where = source(path, line)
            node, message = event.get('node') or {}, event.get('message') or {}
            if node.get('kind') == 'inference':
                active_request = (path.name, node.get('id'))
                latency = ((node.get('result') or {}).get('data') or {}).get('latency')
                if latency:
                    runtime_latency[active_request] = numbers(latency)
            if message.get('role') == 'dial_state':
                state = message.get('patch', message.get('state')) or {}
                controls = state.get('inference_controls')
                if isinstance(controls, dict):
                    # Fixed metadata fields only: never copy an arbitrary request body or headers.
                    active_controls = controls_metadata(controls)
                policy = state.get('adaptive_policy') or {}
                result = policy.get('result') or {}
                if result.get('state') == 'response received':
                    if active_request is None:
                        gaps.append({'kind': 'request_identity_unavailable', **where})
                    else:
                        request = requests.setdefault(active_request, {'source': where})
                        if active_controls and active_controls.get('model') != 'unknown' and active_controls.get('model') == policy.get('model'):
                            request['effective_controls'] = active_controls.copy()
                        request.update(model=label(policy.get('model')), result=numbers(result),
                                       finish_reason=label(result.get('finish_reason')))
                        selection = policy.get('selection') or {}
                        request.setdefault('selection', {**numbers(selection),
                            'reasoning_effort': label(selection.get('reasoning_effort'))})
                        request.setdefault('phase', label((policy.get('selection') or {}).get('phase')))
                if state.get('reliability'):
                    reliability = state['reliability']
                    adaptive = {'stats': numbers(reliability.get('stats')),
                                'available_tools': [label(x) for x in reliability.get('current_tools', [])]}
                if state.get('completion_evidence'):
                    evidence = state['completion_evidence']
                    completion = {k: label(evidence.get(k)) for k in ('execution_outcome', 'verification_coverage', 'visual_evidence')}
            if message.get('role') == 'assistant':
                for call in message.get('tool_calls') or []:
                    function = call.get('function') or {}
                    tool_start((path.name, call.get('id')), function.get('name'), function.get('arguments'), where, event.get('ts'))
            elif message.get('role') == 'tool':
                tool_end((path.name, message.get('tool_call_id')), str(message.get('content', '')),
                         message, where, event.get('ts'))

    # OMP stdout is a decision stream. Dial's final result also includes runtime-only checks.
    stdout = trial / 'agent/stdout.jsonl'
    stdout_records = list(records(stdout))
    final_result_line = next((line for line, event in reversed(stdout_records) if event.get('type') == 'result'), None)
    for line, event in stdout_records:
        where = source(stdout, line)
        kind, message = event.get('type'), event.get('message') or {}
        if kind == 'result' and line == final_result_line:
            ledger = event.get('tool_calls', [])
            if not isinstance(ledger, list):
                gaps.append({'kind': 'malformed_tool_ledger', **where})
                continue
            for index, call in enumerate(ledger):
                entry_source = {**where, 'json_pointer': f'/tool_calls/{index}'}
                if not isinstance(call, dict) or not isinstance(call.get('call_id'), str):
                    gaps.append({'kind': 'malformed_tool_ledger', **entry_source})
                    continue
                matches = [value for key, value in tools.items()
                           if key[0] != 'ledger' and key[1] == call['call_id']]
                if len(matches) > 1:
                    gaps.append({'kind': 'ambiguous_tool_identity', **entry_source})
                    continue
                if matches:
                    record = matches[0]
                else:
                    record = {'tool': label(call.get('name')),
                              'origin': 'runtime_focused_check' if call['call_id'].startswith('focused-check:') else 'ledger_only',
                              'call_source': None, 'result_source': None, 'roundtrip_ms': None, 'is_error': None}
                    tools[('ledger', line, index)] = record
                record['ledger_source'] = entry_source
                seconds = number(call.get('seconds'))
                record['execution_duration_ms'] = seconds * 1000 if seconds is not None and seconds >= 0 else None
                error = call.get('is_error')
                if isinstance(error, bool):
                    if record.get('is_error') is not None and record['is_error'] != error:
                        gaps.append({'kind': 'tool_error_status_conflict', **entry_source})
                        record['is_error'] = None
                    else:
                        record['is_error'] = error
        elif not journals and kind == 'message_end' and message.get('role') == 'assistant':
            requests[('omp', line)] = {'source': where, 'model': label(message.get('model')),
                                     'reported_usage': numbers(message.get('usage')),
                                     'finish_reason': label(message.get('stopReason')), 'phase': 'unknown'}
        elif not journals and kind == 'tool_execution_start':
            tool_start(('omp', event.get('toolCallId')), event.get('toolName'), event.get('args'), where)
        elif not journals and kind == 'tool_execution_end':
            text = json.dumps(event.get('result'), ensure_ascii=True)
            tool_end(('omp', event.get('toolCallId')), text, event, where, omp=True)

    for key, request in requests.items():
        if key in runtime_latency:
            request['observed_latency_ms'] = runtime_latency[key]
        else:
            gaps.append({'kind': 'request_latency_unavailable', 'source': request['source']})
    if not requests:
        gaps.append({'kind': 'per_request_trace_unavailable'})
    if not journals and not any(key[0] == 'omp' for key in tools):
        gaps.append({'kind': 'decision_journal_unavailable'})
    if any(r.get('roundtrip_ms') is None for r in tools.values()):
        gaps.append({'kind': 'some_tool_durations_unavailable'})
    if any(not t.get('result_source') and not t.get('ledger_source') for t in tools.values()):
        gaps.append({'kind': 'some_tool_results_unavailable'})
    ledger_only = sum(bool(t.get('ledger_source')) and not t.get('result_source') for t in tools.values())
    if ledger_only:
        gaps.append({'kind': 'tool_bodies_unavailable_from_ledger', 'calls': ledger_only})
    for path in (trial / 'agent/stderr.txt',):
        data = read(path, required=True)
        if data:
            for line, text in enumerate(data.decode(errors='replace').splitlines(), 1):
                if labels(text):
                    stderr_events.append({'source': source(path, line), 'observations': labels(text)})
    for name in ('agent/instruction.txt', 'agent/version.txt', 'verifier/test-stdout.txt', 'verifier/reward.txt'):
        read(trial / name)
    timing, result = {}, {}
    data = read(trial / 'result.json', required=True)
    if data:
        try:
            result = json.loads(data)
            for name in ('environment_setup', 'agent_setup', 'agent_execution', 'verifier'):
                interval = result.get(name) or {}
                timing[name + '_ms'] = milliseconds(interval.get('started_at'), interval.get('finished_at'))
            timing['end_to_end_ms'] = milliseconds(result.get('started_at'), result.get('finished_at'))
        except (ValueError, AttributeError):
            gaps.append({'kind': 'malformed_json', **source(trial / 'result.json')})
    verifier, metrics, raw_metrics = None, {}, {}
    for name in ('verifier/ctrf.json', 'agent/metrics.json'):
        data = read(trial / name, required=True)
        if data:
            try:
                parsed = json.loads(data)
                if name.endswith('ctrf.json'):
                    results = parsed.get('results') or {}
                    verifier = {'summary': numbers(results.get('summary')),
                                'tests': [{'name': label(t.get('name')),
                                           'status': t.get('status') if t.get('status') in ('passed', 'failed', 'pending', 'skipped', 'other') else 'unknown',
                                           'duration_seconds': number(t.get('duration')),
                                           'retries': number(t.get('retries'))} for t in results.get('tests', [])]}
                else:
                    if not isinstance(parsed, dict) or not isinstance(parsed.get('usage', {}), dict):
                        raise ValueError('Invalid metrics shape')
                    raw_metrics = parsed
                    metrics = {'usage': numbers(parsed.get('usage'))}
            except (ValueError, AttributeError, TypeError):
                gaps.append({'kind': 'malformed_json', **source(trial / name)})
    unknown_errors = sum(t.get('is_error') is None for t in tools.values())
    if unknown_errors:
        gaps.append({'kind': 'tool_error_classification_missing', 'calls': unknown_errors})
    known_errors = sum(t.get('is_error') is True for t in tools.values())
    aggregate_errors = (metrics.get('usage') or {}).get('tool_errors')
    if aggregate_errors is not None and (unknown_errors or aggregate_errors != known_errors):
        gaps.append({'kind': 'tool_error_count_mismatch', 'aggregate': aggregate_errors, 'known_trace_errors': known_errors})
    observed_tools = sum(bool(t.get('result_source') or t.get('ledger_source')) for t in tools.values())
    expected = (metrics.get('usage') or {}).get('tool_calls')
    if expected is not None and expected != observed_tools:
        gaps.append({'kind': 'tool_count_mismatch', 'aggregate': expected, 'trace': observed_tools})
    error_count_complete = unknown_errors == 0 and expected is not None and expected == observed_tools
    expected_requests = (metrics.get('usage') or {}).get('requests')
    if expected_requests is not None and expected_requests != len(requests):
        gaps.append({'kind': 'request_count_mismatch', 'aggregate': expected_requests, 'trace': len(requests)})
    metadata = trial_metadata(result if isinstance(result, dict) else {}, raw_metrics)
    if metadata['metric_agent'] and metadata['reported_agent'] and metadata['metric_agent'] != metadata['reported_agent']:
        gaps.append({'kind': 'agent_identity_mismatch', 'source': metadata['source']})
    if completion is None:
        gaps.append({'kind': 'completion_evidence_unavailable'})
    return {'schema_version': 1, 'privacy': 'Metadata only; raw logs remain private and may contain secrets. Do not publish them.',
            'interpretation': 'Pattern labels and repeated arguments are observations, not causes or proof of wasted work. Durations are observed call-to-result intervals, not process-only time. Token fields retain producer semantics; cache traffic is not billed cost.',
            'source_inventory': inventory, 'requests': list(requests.values()), 'tools': list(tools.values()),
            'stderr_events': stderr_events, 'final_reliability': adaptive or None, 'completion_evidence': completion,
            'trial_metadata': metadata, 'reported_efficiency': efficiency_metadata(raw_metrics.get('usage') or {}),
            'verifier': verifier, 'stage_durations_ms': timing, 'coverage_gaps': gaps,
            'observed_counts': {'requests': len(requests), 'tool_results': observed_tools,
                                'tool_errors': known_errors if error_count_complete else None}}


def measurement(values, expected_samples=None):
    """Observed descriptive statistics; partial sums never masquerade as complete totals."""
    valid = [v for v in values if number(v) is not None and not isinstance(v, bool) and v >= 0]
    expected = len(values) if expected_samples is None else expected_samples
    complete = bool(expected) and len(valid) == expected
    observed = number(sum(valid)) if valid else None
    return {'samples': len(valid), 'expected_samples': expected, 'complete': complete,
            'observed_sum': observed, 'total': observed if complete else None,
            'min': min(valid) if valid else None, 'median': median(valid), 'p95': percentile95(valid),
            'max': max(valid) if valid else None}


def summarize_agent(rows):
    diagnostics = [r.get('diagnostics') or {} for r in rows]
    measures = {'agent_elapsed_sec': measurement([r.get('elapsed_sec') for r in rows])}
    for key in USAGE_COUNTERS:
        measures['usage.' + key] = measurement([(r.get('usage') or {}).get(key) for r in rows])
    for stage in ('environment_setup', 'agent_setup', 'agent_execution', 'verifier', 'end_to_end'):
        measures[stage + '_ms'] = measurement([(d.get('stage_durations_ms') or {}).get(stage + '_ms') for d in diagnostics])
    for group, fields in EFFICIENCY_FIELDS.items():
        for key in fields:
            measures[group + '.' + key] = measurement([
                ((d.get('reported_efficiency') or {}).get(group) or {}).get(key) for d in diagnostics])
    for phase in PHASES:
        measures['requests_by_phase.' + phase] = measurement([
            (((d.get('reported_efficiency') or {}).get('projection_bytes') or {}).get('requests_by_phase') or {}).get(phase)
            for d in diagnostics])
    requests = [request for d in diagnostics for request in d.get('requests', [])]
    expected = measures['usage.requests']['total']
    latency = measurement([(r.get('observed_latency_ms') or {}).get('total_ms') for r in requests],
                          expected_samples=expected if expected is not None else len(requests))
    reconciled = all(type((r.get('usage') or {}).get('requests')) is int
                     and (r.get('usage') or {})['requests'] == len(d.get('requests', []))
                     for r, d in zip(rows, diagnostics))
    if expected is None or not reconciled:
        latency['complete'], latency['total'] = False, None
        if expected is None:
            latency['expected_samples'] = None
    tools = [t for d in diagnostics for t in d.get('tools', [])]
    by_tool = {}
    for name in sorted({t.get('tool', 'unknown') for t in tools}):
        selected = [t for t in tools if t.get('tool', 'unknown') == name]
        by_tool[name] = {'calls': len(selected), 'known_errors': sum(t.get('is_error') is True for t in selected),
                         'unknown_error_calls': sum(t.get('is_error') is None for t in selected),
                         'repeated_arguments': sum((t.get('same_arguments_seen_before') or 0) > 0 for t in selected),
                         'observations': dict(Counter(x for t in selected for x in t.get('observations', []))),
                         'origins': dict(Counter(t.get('origin', 'unknown') for t in selected)),
                         'call_to_result_ms': measurement([t.get('roundtrip_ms') for t in selected]),
                         'execution_duration_ms': measurement([t.get('execution_duration_ms') for t in selected])}
    return {'trials': len(rows), 'measurements': measures, 'request_latency_ms': latency, 'tools': by_tool,
            'coverage_gaps': dict(Counter(g['kind'] for d in diagnostics for g in d.get('coverage_gaps', [])))}


def job_inventory(root, requested_batch):
    jobs = []
    for job in sorted((root / 'jobs').iterdir()) if (root / 'jobs').is_dir() else []:
        if not job.is_dir() or (requested_batch and not job.name.startswith(requested_batch + '-')):
            continue
        inventory, gaps, parsed = [], [], {}
        data = read_source(job / 'result.json', root, inventory, gaps, required=True)
        if data:
            try:
                parsed = json.loads(data)
                if not isinstance(parsed, dict) or not isinstance(parsed.get('stats', {}), dict):
                    raise ValueError('Invalid job result')
            except (ValueError, UnicodeDecodeError):
                parsed = {}
                gaps.append({'kind': 'malformed_json', 'path': (job / 'result.json').relative_to(root).as_posix()})
        counts = {k: number((parsed.get('stats') or {}).get(k)) for k in (
            'n_completed_trials', 'n_errored_trials', 'n_running_trials', 'n_pending_trials', 'n_cancelled_trials', 'n_retries')}
        counts['n_total_trials'] = number(parsed.get('n_total_trials'))
        found = len(list(job.glob('*/result.json'))) if job.resolve().is_relative_to(root.resolve()) else 0
        completed = counts['n_completed_trials']
        if completed is not None and found != completed:
            gaps.append({'kind': 'job_trial_result_count_mismatch', 'completed': completed, 'discovered': found})
        if not found:
            gaps.append({'kind': 'job_without_trial_results'})
        jobs.append({'job': label(job.name), 'counts': counts, 'discovered_trial_results': found,
                     'source_inventory': inventory, 'coverage_gaps': gaps})
    return jobs


def report_overview(rows, expected_tasks, agents, root=None, requested_batch=None):
    """Surface correctness, attribution and missingness without pooling batches or hiding attempts."""
    def agent(row):
        return row.get('agent') or ((row.get('diagnostics') or {}).get('trial_metadata') or {}).get('reported_agent') or 'unknown'

    def task(row):
        return row['task'].rsplit('/', 1)[-1]

    batches = {}
    for batch in sorted({r['batch'] for r in rows} | ({requested_batch} if requested_batch else set())):
        selected = [r for r in rows if r['batch'] == batch and not r.get('excluded_setup_failure')]
        expected = {(name.rsplit('/', 1)[-1], a) for name in expected_tasks for a in agents}
        pairs = Counter((task(r), agent(r)) for r in selected)
        tasks, contracts = {}, {}
        for name in sorted({t for t, _ in expected} | {task(r) for r in selected}):
            tasks[name] = {}
            checksums = [((r.get('diagnostics') or {}).get('trial_metadata') or {}).get('task_checksum')
                         for r in selected if task(r) == name]
            known = sorted({c for c in checksums if c is not None})
            contracts[name] = {'observed_checksums': known, 'samples': sum(c is not None for c in checksums),
                               'expected_samples': len(checksums),
                               'status': 'mismatch' if len(known) > 1 else
                                         'matched' if checksums and all(c is not None for c in checksums) else 'unknown'}
            for label_ in sorted(set(agents) | {agent(r) for r in selected if task(r) == name}):
                attempts = []
                for row in selected:
                    if task(row) != name or agent(row) != label_:
                        continue
                    diag = row.get('diagnostics') or {}
                    metadata = diag.get('trial_metadata') or {}
                    tests = (diag.get('verifier') or {}).get('tests') or []
                    attempts.append({'job': row['job'], 'reward': (row.get('reward') or {}).get('reward'),
                        'exception_present': bool(row.get('exception')), 'exit_code': metadata.get('exit_code'),
                        'identity': metadata, 'elapsed_sec': row.get('elapsed_sec'),
                        'usage': {k: (row.get('usage') or {}).get(k) for k in USAGE_COUNTERS},
                        'official_test_counts': dict(Counter(t['status'] for t in tests)),
                        'failed_tests': [t['name'] for t in tests if t['status'] == 'failed'],
                        'verifier_retries': measurement([t.get('retries') for t in tests])['total'],
                        'completion_evidence': diag.get('completion_evidence'),
                        'coverage_gaps': dict(Counter(g['kind'] for g in diag.get('coverage_gaps', [])))})
                tasks[name][label_] = attempts
        batches[batch] = {
            'expected_trials': len(expected), 'observed_trials': len(selected),
            'excluded_trials': sum(r['batch'] == batch and bool(r.get('excluded_setup_failure')) for r in rows),
            'missing_pairs': [{'task': t, 'agent': a} for t, a in sorted(expected - pairs.keys())],
            'unexpected_pairs': [{'task': t, 'agent': a} for t, a in sorted(pairs.keys() - expected)],
            'duplicate_pairs': [{'task': t, 'agent': a, 'trials': count} for (t, a), count in sorted(pairs.items()) if count > 1],
            'task_contracts': contracts, 'tasks': tasks, 'agents': {a: summarize_agent([r for r in selected if agent(r) == a])
                                      for a in sorted(set(agents) | {agent(r) for r in selected})}}
    return {'schema_version': 1, 'batches': batches,
            'jobs': job_inventory(Path(root), requested_batch) if root is not None else [],
            'interpretation': 'Descriptive retained-data summaries, not causal estimates. Samples and expected samples show measurement coverage; total is null when incomplete. Request/tool sums may overlap and are not wall-clock overhead. p95 uses nearest rank and is the maximum for small samples. Provider retries, Harbor retries, tool errors and verifier retries are separate layers. Unknown costs, effective container resources, per-trial executable hashes and absent wire payloads cannot be reconstructed from these logs.'}


def render_overview(overview):
    """Readable companion to the structured overview, with the same unknown/coverage semantics."""
    def value(item):
        if item is None:
            return 'unknown'
        if isinstance(item, float):
            return f'{item:.3f}'
        return str(item).replace('|', '\\|').replace('\n', ' ')

    lines = ['# Harbor retained-data overview', '', overview['interpretation'], '']
    for name, batch in overview['batches'].items():
        lines.extend([f'## Batch: {name}', '',
            f"Trials: {batch['observed_trials']} observed / {batch['expected_trials']} expected; {batch['excluded_trials']} excluded.", ''])
        for kind in ('missing_pairs', 'duplicate_pairs', 'unexpected_pairs'):
            for pair in batch[kind]:
                lines.append(f"{kind.split('_')[0].capitalize()}: {pair['task']} / {pair['agent']}" +
                             (f" ({pair['trials']} trials)" if 'trials' in pair else ''))
        lines.extend(['', '### Task outcomes', '',
            '| Task | Agent | Reward | Exit | Agent seconds | Requests | Input incl. cache | Output | Failed official tests | Verifier retries |',
            '| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: |'])
        identities = []
        for task, agents in batch['tasks'].items():
            for agent, trials in agents.items():
                for trial in trials:
                    usage = trial['usage']
                    fields = [task, agent, trial['reward'], trial['exit_code'], trial['elapsed_sec'],
                              usage['requests'], usage['input_total'], usage['output'],
                              ', '.join(trial['failed_tests']) or ('none' if trial['official_test_counts'] else 'unknown'),
                              trial['verifier_retries']]
                    lines.append('| ' + ' | '.join(value(x) for x in fields) + ' |')
                    identity = trial['identity']
                    identities.extend([f"- {trial['job']}: reported agent `{value(identity.get('reported_agent'))}`, "
                        f"model `{value(identity.get('reported_model'))}`, provider `{value(identity.get('reported_provider'))}`; "
                        f"exception present: {trial['exception_present']}; checksum status: {batch['task_contracts'][task]['status']}.",
                        '- Recorded constraints (overrides, not measured resources): ' + value(json.dumps(identity.get('constraints', {}), sort_keys=True)),
                        '- Coverage gaps: ' + value(json.dumps(trial['coverage_gaps'], sort_keys=True)), ''])
        lines.extend(['', '### Identity and recorded constraints', '', *identities])
        for agent, summary in batch['agents'].items():
            lines.extend([f'### Measurements: {agent}', '',
                '| Metric (unit in name) | Samples / expected | Complete total | Observed partial/full sum | Median | Nearest-rank p95 |',
                '| --- | --- | ---: | ---: | ---: | ---: |'])
            for key, measured in {**summary['measurements'], 'request_latency_ms': summary['request_latency_ms']}.items():
                fields = [key, f"{measured['samples']} / {value(measured['expected_samples'])}",
                          measured['total'], measured['observed_sum'], measured['median'], measured['p95']]
                lines.append('| ' + ' | '.join(value(x) for x in fields) + ' |')
            lines.extend(['', '| Tool | Calls | Known errors | Unknown error status | Repeated arguments | Observations |',
                          '| --- | ---: | ---: | ---: | ---: | --- |'])
            for tool, detail in summary['tools'].items():
                fields = [tool, detail['calls'], detail['known_errors'], detail['unknown_error_calls'],
                          detail['repeated_arguments'], json.dumps(detail['observations'], sort_keys=True)]
                lines.append('| ' + ' | '.join(value(x) for x in fields) + ' |')
            lines.extend(['', 'Coverage-gap counts: ' + value(json.dumps(summary['coverage_gaps'], sort_keys=True)), ''])
    lines.extend(['## Harbor job status and retry counts', '',
                  '| Job | Trial results found | Total declared | Completed | Errored | Running | Pending | Cancelled | Harbor retries | Gaps |',
                  '| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |'])
    for job in overview['jobs']:
        counts = job['counts']
        fields = [job['job'], job['discovered_trial_results'], *(counts.get(k) for k in (
            'n_total_trials', 'n_completed_trials', 'n_errored_trials', 'n_running_trials', 'n_pending_trials', 'n_cancelled_trials', 'n_retries')),
            ', '.join(g['kind'] for g in job['coverage_gaps']) or 'none']
        lines.append('| ' + ' | '.join(value(x) for x in fields) + ' |')
    return '\n'.join(lines) + '\n'
