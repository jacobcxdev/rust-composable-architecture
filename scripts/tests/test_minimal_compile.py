import copy
import itertools
import json
import pathlib
import os
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from contextlib import ExitStack

from test_composition_runner import runner
from test_composition_performance import composition_performance as evidence

PROTOCOL = pathlib.Path(__file__).resolve().parents[2] / 'docs/performance/composable-minimal-compile-protocol-v1.json'


class MinimalCompileTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(callable(getattr(evidence, 'load_minimal_compile_protocol', None)),
                        'minimal compile admission is missing')
        self.protocol = evidence.load_minimal_compile_protocol(PROTOCOL)

    def rows(self):
        return [dict(item, counters=dict(real_seconds=2.0, user_seconds=1.0,
                    system_seconds=0.5, maximum_resident_set_bytes=100))
                for item in evidence.minimal_compile_plan(self.protocol)]

    def test_schedule_balances_all_engine_orders_and_alternates_workloads(self):
        plan = evidence.minimal_compile_plan(self.protocol)
        self.assertEqual(len(plan), 36)
        orders = []
        for block in range(1, 7):
            batch = [row for row in plan if row['block'] == block]
            self.assertEqual([row['workload'] for row in batch],
                             (['check'] * 3 + ['release'] * 3) if block % 2 else
                             (['release'] * 3 + ['check'] * 3))
            orders.append(tuple(row['engine'] for row in batch[:3]))
        self.assertEqual(set(orders), set(itertools.permutations(['composable', 'rca-0.7', 'rca'])))

    def test_descriptive_summary_preserves_cpu_and_zero(self):
        rows = self.rows()
        for row in rows:
            if row['engine'] == 'composable':
                row['counters']['real_seconds'] = 0.0
        result = evidence.evaluate_minimal_compile(rows, self.protocol)
        self.assertFalse(result['qualified_comparison'])
        self.assertFalse(result['regression_pass'])
        self.assertEqual(result['observations'], rows)
        self.assertEqual(result['summaries']['rca']['check']['cpu_seconds']['median'], 1.5)
        self.assertEqual(result['ratios']['composable']['check']['real_seconds'], [None] * 6)
        for row in rows:
            if row['engine'] == 'rca':
                row['counters']['real_seconds'] = 0.0
        result = evidence.evaluate_minimal_compile(rows, self.protocol)
        self.assertEqual(result['ratios']['rca-0.7']['check']['real_seconds'], [None] * 6)
        self.assertFalse(result['timing_qualified'])

    def test_malformed_missing_duplicate_and_changed_protocol_rejected(self):
        rows = self.rows()
        for invalid in (None, {}, rows[:-1], rows + [rows[0]], list(reversed(rows))):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                evidence.evaluate_minimal_compile(invalid, self.protocol)
        for value in (-1, True, float('nan'), float('inf'), '2'):
            bad = copy.deepcopy(rows)
            bad[0]['counters']['real_seconds'] = value
            with self.subTest(value=value), self.assertRaises(ValueError):
                evidence.evaluate_minimal_compile(bad, self.protocol)
        for key in ('engine', 'counters'):
            bad = copy.deepcopy(rows)
            del bad[0][key]
            with self.assertRaises(ValueError):
                evidence.evaluate_minimal_compile(bad, self.protocol)
        bad = copy.deepcopy(rows)
        bad[0]['extra'] = 1
        with self.assertRaises(ValueError):
            evidence.evaluate_minimal_compile(bad, self.protocol)
        protocol = copy.deepcopy(self.protocol)
        protocol['process_timeout_seconds'] += 1
        with self.assertRaises(ValueError):
            evidence.evaluate_minimal_compile(rows, protocol)


class CompileCommandTests(unittest.TestCase):
    def test_qualification_rejects_rewritten_inputs_and_preparation(self):
        protocol = evidence.load_minimal_compile_protocol(PROTOCOL)
        for mutation in ('inputs.json', 'protocol.json', 'plan.json', 'preparation', 'dependency-hashes.json'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = pathlib.Path(temporary)
                stage = root / 'capture'
                tool = root / 'tool'
                tool.write_text('controlled tool identity')
                source = dict(revision='a' * 40, hashes={str(tool): evidence._sha256(tool)})
                tools = {name: str(tool) for name in ('cargo', 'rustc', 'rustdoc')}
                def environment(cwd, cargo_home):
                    return ({**runner.COUNTER_RELEASE_ENV, 'CARGO_HOME': str(cargo_home),
                             'RUSTC_WRAPPER': '', 'RUSTC_WORKSPACE_WRAPPER': ''}, tools)
                calls = []
                def execute(command, log, env, cwd, **_kwargs):
                    calls.append(log)
                    log.parent.mkdir(parents=True, exist_ok=True)
                    log.write_text('fetched')
                    log.with_suffix('.stderr.txt').write_text('')
                    runner.write_json(log.with_suffix('.command.json'), dict(command=command, cwd=str(cwd)))
                    runner.write_json(log.with_suffix('.exit.json'), dict(returncode=0))
                    if 'metadata' in command:
                        fixture = pathlib.Path(command[command.index('--manifest-path') + 1]).parent.name
                        engine = next(k for k, v in runner.COUNTER_FIXTURES.items() if v == fixture)
                        packages, nodes = [], []
                        if engine in protocol['pins']:
                            pin = protocol['pins'][engine]
                            packages = [dict(id='legacy', name='composable',
                                source=f"git+{protocol['repositories'][engine]}?rev={pin}#{pin}")]
                            nodes = [dict(id='legacy', features=[])]
                        log.write_text(json.dumps(dict(packages=packages, resolve=dict(nodes=nodes))))
                    if len(calls) == 3 and mutation != 'dependency-hashes.json':
                        target = (calls[0] if mutation == 'preparation' else stage / mutation)
                        target.write_text('{}')
                class StopBeforeMeasurement:
                    def __init__(self, *_args):
                        if mutation == 'dependency-hashes.json':
                            (stage / mutation).write_text('{"changed":true}')
                    def idle(self, *_args, **_kwargs):
                        raise AssertionError('tampered preparation reached measurement')
                with ExitStack() as stack:
                    stack.enter_context(patch.object(runner, 'counter_source_identity', return_value=source))
                    stack.enter_context(patch.object(runner, 'canonical_host', return_value={'identity': {}}))
                    stack.enter_context(patch.object(runner, 'counter_environment', side_effect=environment))
                    stack.enter_context(patch.object(runner, 'read', return_value='controlled version'))
                    stack.enter_context(patch.object(runner, '_counter_command', side_effect=execute))
                    stack.enter_context(patch.object(runner, 'counter_dependency_identity', return_value={}))
                    stack.enter_context(patch.object(runner, 'HostMonitor', StopBeforeMeasurement))
                    with self.assertRaisesRegex(ValueError, 'input changed'):
                        runner.qualify_minimal_compile(root / 'published', stage)
                self.assertTrue((stage / 'failure.json').is_file())
                self.assertFalse((stage / 'complete.json').exists())

    @unittest.skipUnless(sys.platform == 'darwin', 'Darwin time flags are required for timeout witness')
    def test_timeout_retains_failure_receipt_without_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            log = root / 'timeout.txt'
            monitor = runner.HostMonitor(root / 'host', {'quiescence': {'process_monitor_interval_seconds': 0.01}})
            # Shell sleep is deliberately outside the native compiler workload.
            with patch.object(monitor, 'idle'), patch.object(monitor, 'sample', return_value={}):
                with self.assertRaises(subprocess.TimeoutExpired):
                    monitor.run(['/bin/sh', '-c', 'sleep 5'], log, root, dict(os.environ),
                                timeout=0.1, counter_receipt=True)
            self.assertNotEqual(json.loads(log.with_suffix('.exit.json').read_text())['returncode'], 0)
            self.assertFalse(log.with_suffix('.resources.json').exists())

    def test_frozen_inputs_and_busy_host_fail_before_first_measurement(self):
        protocol = evidence.load_minimal_compile_protocol(PROTOCOL)
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / 'source'
            source.write_text('before')
            frozen = {'source': {str(source): evidence._sha256(source)}}
            source.write_text('changed')
            monitor = runner.HostMonitor(root / 'host', protocol)
            with patch.object(monitor, 'run', side_effect=AssertionError('measurement started')):
                with self.assertRaisesRegex(ValueError, 'input changed'):
                    runner.capture_minimal_compile(root / 'capture', root, protocol,
                        {'CARGO_HOME': str(root / 'cargo')}, {'cargo': '/cargo'}, monitor, frozen)
                with patch.object(monitor, 'idle', side_effect=ValueError('busy host')):
                    with self.assertRaisesRegex(ValueError, 'busy host'):
                        runner.capture_minimal_compile(root / 'capture', root, protocol,
                            {'CARGO_HOME': str(root / 'cargo')}, {'cargo': '/cargo'}, monitor, {})

    def test_complete_schedule_reloads_and_rejects_changed_receipts(self):
        protocol = evidence.load_minimal_compile_protocol(PROTOCOL)
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            staging = root / 'capture'
            raw = 'real 2.00\nuser 1.00\nsys 0.50\n100 maximum resident set size\n'
            for label in ('page reclaims', 'page faults', 'swaps', 'block input operations',
                          'block output operations', 'voluntary context switches',
                          'involuntary context switches', 'instructions retired', 'cycles elapsed',
                          'peak memory footprint'):
                raw += f'0 {label}\n'
            calls = []
            class Monitor:
                def idle(self, *_args, **_kwargs):
                    pass
                def run(self, command, log, cwd, env, **options):
                    self_target = pathlib.Path(env['CARGO_TARGET_DIR'])
                    if self_target.exists():
                        raise AssertionError('target reused')
                    if not options['counter_receipt'] or options['timeout'] != 600:
                        raise AssertionError('counter binding or deadline omitted')
                    calls.append((command, env))
                    log.parent.mkdir(parents=True, exist_ok=True)
                    log.write_text('finished\n')
                    runner.write_json(log.with_suffix('.command.json'), dict(command=command, cwd=str(cwd),
                        target=str(self_target), counter=str(log.with_suffix('.time.txt'))))
                    runner.write_json(log.with_suffix('.exit.json'), dict(returncode=0))
                    log.with_suffix('.time.txt').write_text(raw)
                    runner.write_json(log.with_suffix('.resources.json'), evidence.parse_darwin_time(raw))
                    binary = self_target / 'aarch64-apple-darwin/release/minimal-counter'
                    binary.parent.mkdir(parents=True)
                    binary.write_text('binary identity')
            def execute(command, log, env, timeout, cwd):
                self.assertEqual(pathlib.Path(command[0]).name, 'minimal-counter')
                log.parent.mkdir(parents=True, exist_ok=True)
                log.write_text('42\n')
                log.with_suffix('.stderr.txt').write_text('')
                runner.write_json(log.with_suffix('.command.json'), dict(command=command, cwd=str(cwd)))
                runner.write_json(log.with_suffix('.exit.json'), dict(returncode=0))
            with patch.object(runner, '_counter_command', side_effect=execute):
                rows, receipts, binaries = runner.capture_minimal_compile(staging, root, protocol,
                    {'CARGO_HOME': str(root / 'cargo-home')}, {'cargo': '/cargo'}, Monitor(), {})
            self.assertEqual(len(calls), 36)
            self.assertEqual(len(binaries), 18)
            self.assertEqual(len({env['CARGO_TARGET_DIR'] for _, env in calls}), 36)
            self.assertEqual(sum(command[1] == 'check' for command, _ in calls), 18)
            self.assertEqual(sum('--release' in command for command, _ in calls), 18)
            inputs = dict(build_cwd=str(root), source_root=str(runner.ROOT),
                          capture_directory=str(staging), tools={'cargo': {'path': '/cargo'}})
            self.assertEqual(runner.reload_minimal_compile(staging, protocol, inputs), rows)
            runner.verify_counter_inputs({'receipts': receipts, 'binaries': binaries})
            receipt = next((staging / 'commands').glob('*.command.json'))
            receipt.write_text('{}')
            with self.assertRaises(ValueError):
                runner.reload_minimal_compile(staging, protocol, inputs)
            with self.assertRaises(ValueError):
                runner.verify_counter_inputs({'receipts': receipts})

    def test_failed_timed_command_retains_bound_exit_without_parsed_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            log = root / 'failed.txt'
            monitor = runner.HostMonitor(root / 'host', {'quiescence': {'process_monitor_interval_seconds': 0.01}})
            with patch.object(monitor, 'idle'), patch.object(monitor, 'sample', return_value={}):
                with self.assertRaises(subprocess.CalledProcessError):
                    monitor.run(['/bin/sh', '-c', 'exit 7'], log, root, dict(os.environ),
                                timeout=5, counter_receipt=True)
            self.assertNotEqual(json.loads(log.with_suffix('.exit.json').read_text())['returncode'], 0)
            self.assertEqual(json.loads(log.with_suffix('.command.json').read_text())['counter'],
                             str(log.with_suffix('.time.txt')))
            self.assertFalse(log.with_suffix('.resources.json').exists())

    def test_capture_failure_does_not_retry_or_continue(self):
        self.assertTrue(callable(getattr(runner, 'capture_minimal_compile', None)),
                        'minimal compile capture is missing')
        protocol = evidence.load_minimal_compile_protocol(PROTOCOL)
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            class FailedMonitor:
                count = 0
                def idle(self, *_args, **_kwargs):
                    pass
                def run(self, *_args, **_kwargs):
                    self.count += 1
                    raise ValueError('failed measured command')
            monitor = FailedMonitor()
            with self.assertRaisesRegex(ValueError, 'failed measured command'):
                runner.capture_minimal_compile(root / 'capture', root, protocol,
                    {'CARGO_HOME': str(root / 'cargo-home')}, {'cargo': '/cargo'}, monitor, {})
            self.assertEqual(monitor.count, 1)

    def test_counter_receipt_rejects_missing_tampered_failed_or_wrong_command(self):
        self.assertTrue(callable(getattr(runner, 'minimal_compile_observation', None)),
                        'compile counter binding is missing')
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            log = root / 'command.txt'
            command = ['/cargo', 'check']
            raw = 'real 2.00\nuser 1.00\nsys 0.50\n100 maximum resident set size\n'
            for label in ('page reclaims', 'page faults', 'swaps', 'block input operations',
                          'block output operations', 'voluntary context switches',
                          'involuntary context switches', 'instructions retired', 'cycles elapsed',
                          'peak memory footprint'):
                raw += f'0 {label}\n'
            def fixture():
                log.write_text('build complete\n')
                log.with_suffix('.time.txt').write_text(raw)
                log.with_suffix('.resources.json').write_text(json.dumps(evidence.parse_darwin_time(raw)))
                log.with_suffix('.command.json').write_text(json.dumps(dict(command=command, cwd=str(root),
                    target=str(root / 'target'), counter=str(log.with_suffix('.time.txt')))))
                log.with_suffix('.exit.json').write_text('{"returncode":0}')
            fixture()
            item = dict(block=1, engine='rca', workload='check')
            row = runner.minimal_compile_observation(log, command, root, root / 'target', item)
            self.assertEqual(row['counters'], dict(real_seconds=2.0, user_seconds=1.0,
                             system_seconds=0.5, maximum_resident_set_bytes=100))
            for suffix, contents in (('.time.txt', raw + 'real 9.00\n'),
                                     ('.resources.json', '{}'), ('.exit.json', '{"returncode":1}'),
                                     ('.exit.json', '{"returncode":1,"returncode":0}'),
                                     ('.command.json', '{}'), ('.time.txt', None)):
                fixture()
                path = log.with_suffix(suffix)
                if contents is None:
                    path.unlink()
                else:
                    path.write_text(contents)
                with self.subTest(suffix=suffix), self.assertRaises((ValueError, OSError)):
                    runner.minimal_compile_observation(log, command, root, root / 'target', item)

    def test_command_requires_fresh_target_and_selects_only_minimal_binary(self):
        self.assertTrue(callable(getattr(runner, 'minimal_compile_command', None)),
                        'minimal compile command construction is missing')
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            target = root / 'target'
            command = runner.minimal_compile_command('/cargo', root / 'Cargo.toml', 'check', target,
                                                     'aarch64-apple-darwin')
            self.assertEqual(command, ['/cargo', 'check', '--manifest-path', str(root / 'Cargo.toml'),
                '--locked', '--offline', '--bin', 'minimal-counter', '--target', 'aarch64-apple-darwin', '-j', '1'])
            release = runner.minimal_compile_command('/cargo', root / 'Cargo.toml', 'release', target,
                                                     'aarch64-apple-darwin')
            self.assertEqual(release[1], 'build')
            self.assertIn('--release', release)
            target.mkdir()
            with self.assertRaises(ValueError):
                runner.minimal_compile_command('/cargo', root / 'Cargo.toml', 'check', target, 'target')


if __name__ == '__main__':
    unittest.main()
