"""Exercise progress at the HTTP/engine boundary without loading model weights."""
import asyncio
import io
import json
from types import SimpleNamespace as NS
import unittest
from unittest import mock

import dial_server as dial


def packets(wire):
    return [json.loads(line[6:]) for line in wire.splitlines() if line.startswith(b"data: {")]


class TokenTests(unittest.TestCase):
    def test_counts_hidden_and_batched_tokens_and_closes_on_interrupt(self):
        closed = []
        def tokens():
            try:
                yield NS(text="", token_count=2)
                yield NS(text="answer", token_count=3, finish_reason="stop")
            finally:
                closed.append(True)
        progress = []
        with mock.patch("time.perf_counter", side_effect=[1.0, 1.1]):
            result = list(dial._observed_tokens(tokens(), progress.append, 42))
        self.assertEqual([p["dial_progress"]["output_tokens"] for p in progress], [2, 5, 5])
        self.assertAlmostEqual(progress[-1]["dial_progress"]["tokens_per_second"], 30)
        self.assertEqual(result[1].text, "answer")
        self.assertEqual(closed, [True])
        iterator = dial._observed_tokens(tokens(), progress.append)
        next(iterator)
        iterator.close()
        self.assertEqual(closed, [True, True])

    def test_fallback_cumulative_counts_are_not_added_twice(self):
        progress = []
        items = [NS(generation_tokens=3), NS(generation_tokens=7, finish_reason="stop")]
        self.assertEqual(list(dial._observed_tokens(iter(items), progress.append)), items)
        self.assertEqual(progress[-1]["dial_progress"]["output_tokens"], 7)

    def test_engine_reported_rate_is_preferred_over_delivery_speed(self):
        progress = []
        list(dial._observed_tokens(iter([NS(generation_tokens=7, generation_tps=22.5)]), progress.append))
        self.assertEqual(progress[-1]["dial_progress"]["tokens_per_second"], 22.5)


class TextProgressTests(unittest.TestCase):
    def test_headers_precede_progress_and_cached_prefill_and_hidden_tokens_reach_wire(self):
        original_callbacks = []
        class Generator:
            def generate(self, request, args, progress_callback=None):
                ctx = NS(prompt=list(range(10)), prompt_cache_count=4)
                def responses():
                    progress_callback(3, 6)
                    yield NS(text="")
                    yield NS(text="ok", finish_reason="stop")
                return ctx, responses()
        class Handler:
            stream = True
            def __init__(self):
                self.wfile = io.BytesIO()
            def handle_completion(self):
                ctx, response = Generator().generate({}, {}, progress_callback=lambda *a: original_callbacks.append(a))
                self.wfile.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                self.result = list(response)
        server = NS(APIHandler=Handler, ResponseGenerator=Generator)
        dial.install_text_progress(server)
        dial.install_text_progress(server)  # Idempotent: no double counting.
        handler = Handler()
        handler.handle_completion()
        wire = handler.wfile.getvalue()
        self.assertTrue(wire.startswith(b"HTTP/1.1"))
        events = [p["dial_progress"] for p in packets(wire)]
        self.assertIn({"phase": "processing", "processed_tokens": 7, "prompt_tokens": 10}, events)
        self.assertEqual(events[-1]["output_tokens"], 2)
        self.assertEqual(original_callbacks, [(3, 6)])
        self.assertEqual([r.text for r in handler.result], ["", "ok"])
        # The request context is gone; unrelated/non-streamed calls remain unmodified.
        _, iterator = Generator().generate({}, {}, progress_callback=lambda *a: None)
        list(iterator)
        self.assertEqual(handler.wfile.getvalue(), wire)


class VisionProgressTests(unittest.IsolatedAsyncioTestCase):
    async def test_processing_is_live_while_worker_runs_and_counters_include_hidden_output(self):
        class Generator:
            def generate(self):
                return NS(prompt_tokens=17), iter([NS(text="", token_count=2), NS(text="ok", token_count=3, finish_reason="stop")])
        app = NS(add_middleware=mock.Mock())
        server = NS(ResponseGenerator=Generator, app=app)
        fallback = NS(stream_generate=lambda: iter([NS(generation_tokens=4, finish_reason="stop")]))
        with mock.patch("importlib.import_module", return_value=fallback):
            dial.install_vision_progress(server)
        app.add_middleware.assert_called_once_with(dial.VisionProgressMiddleware)
        wire = []
        async def send(message):
            wire.append(message)
        async def request(scope, receive, send):
            await send({"type":"http.response.start", "status":200, "headers":[(b"content-type", b"text/event-stream")]})
            ctx, iterator = await asyncio.to_thread(Generator().generate)
            await asyncio.sleep(0.1)
            self.assertTrue(any(p["dial_progress"].get("prompt_tokens") == 17 for p in packets(b"".join(m.get("body", b"") for m in wire))))
            first = await asyncio.to_thread(next, iterator)
            self.assertEqual(first.text, "")
            await asyncio.sleep(0.1)
            await asyncio.to_thread(next, iterator)
            await send({"type":"http.response.body", "body":b'data: {"choices":[{"delta":{"content":"ok"}}]}\n\n', "more_body":True})
            await send({"type":"http.response.body", "body":b"data: [DONE]\n\n", "more_body":False})
            iterator.close()
        middleware = dial.VisionProgressMiddleware(request)
        await middleware({"type":"http", "path":"/v1/chat/completions"}, None, send)
        body = b"".join(m.get("body", b"") for m in wire)
        events = [p["dial_progress"] for p in packets(body) if "dial_progress" in p]
        self.assertEqual(events[0]["phase"], "processing")
        self.assertEqual(events[-1]["output_tokens"], 5)
        self.assertIn(b'"content":"ok"', body)
        self.assertTrue(body.endswith(b"data: [DONE]\n\n"))
        self.assertIsNone(dial.VisionProgressMiddleware.context.get())
        count = len(wire)
        await asyncio.sleep(0.1)
        self.assertEqual(len(wire), count)

    async def test_json_responses_and_cancellation_leave_no_monitor_or_context(self):
        from contextvars import ContextVar
        dial.VisionProgressMiddleware.context = ContextVar("test", default=None)
        messages = []
        async def send(message):
            messages.append(message)
        async def response(scope, receive, send):
            await send({"type":"http.response.start", "headers":[(b"content-type", b"application/json")]})
            await send({"type":"http.response.body", "body":b'{"ok":true}'})
        await dial.VisionProgressMiddleware(response)({"type":"http", "path":"/chat/completions"}, None, send)
        self.assertEqual(len(messages), 2)
        async def cancelled(scope, receive, send):
            await send({"type":"http.response.start", "headers":[(b"content-type", b"text/event-stream")]})
            raise asyncio.CancelledError()
        with self.assertRaises(asyncio.CancelledError):
            await dial.VisionProgressMiddleware(cancelled)({"type":"http", "path":"/chat/completions"}, None, send)
        self.assertIsNone(dial.VisionProgressMiddleware.context.get())
        count = len(messages)
        await asyncio.sleep(0.1)
        self.assertEqual(len(messages), count)


class _FakeGenerator:
    """Mirrors the pinned MLX-VLM hook signatures; the step script stands in for engine counters."""
    def __init__(self, steps):
        self.steps = list(steps)
    def generate(self, prompt, images=None, audio=None, args=None, videos=None):
        return NS(prompt_tokens=10), iter([])
    def _log_prefill_started(self, request, *, backend):
        return {"request_id": request.request_id, "prefill_processed": -1}
    def _log_prefill_progress(self, batch_gen, active):
        for uid, processed in self.steps.pop(0).items():
            if uid in active and processed > active[uid].get("prefill_processed", -1):
                active[uid]["prefill_processed"] = processed


class VisionPrefillTests(unittest.TestCase):
    def setUp(self):
        patcher = mock.patch("importlib.import_module",
            return_value=NS(stream_generate=lambda *a, **k: iter([])))
        patcher.start()
        self.addCleanup(patcher.stop)

    def install(self, steps):
        generator = type("Generator", (_FakeGenerator,), {})
        server = NS(ResponseGenerator=generator, app=NS(add_middleware=mock.Mock()))
        dial.install_vision_progress(server)
        return generator(steps)

    @staticmethod
    def recorder(state):
        state.publish = mock.Mock(wraps=state.publish)
        return state

    @staticmethod
    def measured(state):
        return [c.args[0]["dial_progress"] for c in state.publish.call_args_list
                if "processed_tokens" in c.args[0]["dial_progress"]]

    @staticmethod
    def request(gen, rid, state):
        args = NS()
        token = dial.VisionProgressMiddleware.context.set(state)
        try:
            gen.generate(prompt="p", args=args)
        finally:
            dial.VisionProgressMiddleware.context.reset(token)
        request = NS(request_id=rid, prompt_tokens=10, args=args)
        return {"request_id": rid, **gen._log_prefill_started(request, backend="continuous_batching")}

    def test_publishes_measured_chunks_once_each(self):
        gen = self.install([{1: 4}, {1: 6}, {1: 6}, {1: 10}])
        state = self.recorder(dial._VisionProgressState())
        active = {1: self.request(gen, "r1", state)}
        for _ in range(4):
            gen._log_prefill_progress(None, active)
        packets = [c.args[0]["dial_progress"] for c in state.publish.call_args_list
                   if "processed_tokens" in c.args[0]["dial_progress"]]
        self.assertEqual([(p["phase"], p["processed_tokens"], p["prompt_tokens"]) for p in packets],
            [("processing", 4, 10), ("processing", 6, 10), ("processing", 10, 10)])

    def test_requests_are_isolated_by_request_id(self):
        gen = self.install([{1: 3, 2: 5}, {1: 7}])
        a = self.recorder(dial._VisionProgressState())
        b = self.recorder(dial._VisionProgressState())
        active = {1: self.request(gen, "a", a), 2: self.request(gen, "b", b)}
        gen._log_prefill_progress(None, active)
        gen._log_prefill_progress(None, active)
        self.assertEqual([p["processed_tokens"] for p in self.measured(a)], [3, 7])
        self.assertEqual([p["processed_tokens"] for p in self.measured(b)], [5])

    def test_cancelled_request_stops_publishing(self):
        gen = self.install([{1: 4}, {1: 8}])
        state = self.recorder(dial._VisionProgressState())
        active = {1: self.request(gen, "r1", state)}
        gen._log_prefill_progress(None, active)
        active.pop(1)  # The batching loop removes cancelled uids before the next step.
        gen._log_prefill_progress(None, active)
        self.assertEqual(len(self.measured(state)), 1)

    def test_publish_failure_is_logged_once_and_hook_is_removed(self):
        gen = self.install([{1: 4}, {1: 8}])
        state = dial._VisionProgressState()
        active = {1: self.request(gen, "r1", state)}
        # Fail only the measured step; the coarse packet at request start must still succeed.
        state.publish = mock.Mock(side_effect=RuntimeError("socket closed"))
        with self.assertLogs(level="WARNING") as logs:
            gen._log_prefill_progress(None, active)
        self.assertIn("r1", logs.output[0])
        self.assertIs(type(gen)._log_prefill_progress, _FakeGenerator._log_prefill_progress)
        gen._log_prefill_progress(None, active)  # Generation continues; telemetry no longer runs.
        self.assertEqual(state.publish.call_count, 1)

    def test_missing_hooks_warn_and_are_not_installed(self):
        class Bare:
            def generate(self, *args, **kwargs):
                return NS(prompt_tokens=1), iter([])
        server = NS(ResponseGenerator=Bare, app=NS(add_middleware=mock.Mock()))
        with self.assertLogs(level="WARNING") as logs:
            dial.install_vision_progress(server)
        self.assertIn("prefill hooks", logs.output[0])
        self.assertFalse(hasattr(Bare, "_log_prefill_started"))


class VisionCachePolicyTests(unittest.TestCase):
    @staticmethod
    def generator(check=None, raises=None, ready=None):
        manager = mock.Mock(name="apc_manager")
        apc = NS(self_check_model_apc=mock.Mock(return_value=check, side_effect=raises), from_env=mock.Mock())
        runtime = NS(apc_manager=manager)
        class Gen:
            kv_bits = None
            apc_mode = "block"
            def __init__(self):
                self.apc_manager = manager
            def wait_until_ready(self, timeout=None):
                if ready is not None:
                    ready()
                return ("model", "processor", "config")
        server = NS(ResponseGenerator=Gen)
        dial.install_vision_cache_policy(server, apc=apc, runtime=runtime)
        return Gen(), apc, runtime, manager

    def test_unsupported_layout_refuses_cache_and_returns_model(self):
        gen, apc, runtime, manager = self.generator(check=NS(ok=False, apc_mode=None, notes=["no make_cache"]))
        with self.assertLogs(level="WARNING") as logs:
            self.assertEqual(gen.wait_until_ready(), ("model", "processor", "config"))
        self.assertIsNone(gen.apc_manager)
        self.assertIsNone(gen.apc_mode)
        self.assertIsNone(runtime.apc_manager)
        manager.close.assert_called_once_with()
        self.assertIn("no make_cache", logs.output[0])
        gen.wait_until_ready()  # The check is one-shot per loaded generator.
        self.assertEqual(apc.self_check_model_apc.call_count, 1)

    def test_supported_layout_keeps_block_cache(self):
        gen, apc, runtime, manager = self.generator(check=NS(ok=True, apc_mode="block", notes=[]))
        gen.wait_until_ready()
        self.assertIs(gen.apc_manager, manager)
        self.assertIs(runtime.apc_manager, manager)
        manager.close.assert_not_called()

    def test_raising_self_check_refuses_cache_with_logged_cause(self):
        gen, apc, runtime, manager = self.generator(raises=RuntimeError("bad layout"))
        with self.assertLogs(level="WARNING") as logs:
            gen.wait_until_ready()
        self.assertIsNone(gen.apc_manager)
        manager.close.assert_called_once_with()
        self.assertTrue(any("APC self-check raised" in line for line in logs.output))

    def test_concurrent_request_waits_until_cache_refusal_finishes(self):
        import threading
        from concurrent.futures import ThreadPoolExecutor
        checking, release, second_ready = (threading.Event() for _ in range(3))
        ready_count = 0
        ready_lock = threading.Lock()
        def ready():
            nonlocal ready_count
            with ready_lock:
                ready_count += 1
                if ready_count == 2:
                    second_ready.set()
        def check(*args, **kwargs):
            checking.set()
            if not release.wait(5):
                raise RuntimeError("test did not release cache validation")
            return NS(ok=False, apc_mode=None, notes=["unsupported"])
        gen, apc, runtime, manager = self.generator(raises=check, ready=ready)
        with ThreadPoolExecutor(max_workers=2) as pool, self.assertLogs(level="WARNING"):
            first = pool.submit(gen.wait_until_ready)
            try:
                self.assertTrue(checking.wait(2))
                second = pool.submit(gen.wait_until_ready)
                self.assertTrue(second_ready.wait(2))
                self.assertFalse(second.done(), "request escaped validation before cache detachment")
            finally:
                release.set()
            first.result(timeout=2)
            second.result(timeout=2)
        self.assertIsNone(gen.apc_manager)
        self.assertIsNone(runtime.apc_manager)
        manager.close.assert_called_once_with()
        apc.self_check_model_apc.assert_called_once()

    def test_upstream_load_error_propagates_without_checking(self):
        apc = NS(self_check_model_apc=mock.Mock(), from_env=mock.Mock())
        class Gen:
            apc_manager = mock.Mock()
            def wait_until_ready(self, timeout=None):
                raise RuntimeError("load failed")
        dial.install_vision_cache_policy(NS(ResponseGenerator=Gen), apc=apc, runtime=NS(apc_manager=None))
        with self.assertRaisesRegex(RuntimeError, "load failed"):
            Gen().wait_until_ready()
        apc.self_check_model_apc.assert_not_called()

    def test_absent_validation_hooks_disable_apc_creation(self):
        apc = NS(from_env=mock.Mock(return_value=mock.Mock()))
        class Bare:
            def wait_until_ready(self, timeout=None):
                return ("m", "p", "c")
        with self.assertLogs(level="WARNING") as logs:
            dial.install_vision_cache_policy(NS(ResponseGenerator=Bare), apc=apc, runtime=NS(apc_manager=None))
        self.assertIsNone(apc.from_env(overrides={"enabled": True}))
        self.assertIn("self-check hooks", logs.output[0])


if __name__ == "__main__":
    unittest.main()
