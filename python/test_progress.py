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


if __name__ == "__main__":
    unittest.main()
