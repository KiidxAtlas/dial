"""Dial's MLX server entry point, embedded by the Rust supervisor.

MLX-LM 0.32.0 assumes tool-parser output always has arguments; malformed model output can raise
KeyError and close the HTTP connection. Preserve such failures as invalid OpenAI tool calls so the
Rust agent can diagnose/retry them. Never synthesize executable arguments for a malformed call.
"""
import json
import logging
import uuid


class _ThinkingSampler:
    """Close a recognized thinking span, then keep sampling the same completion normally."""
    def __init__(self, sampler, tokenizer, limits, initial_state, max_tokens, force_token):
        import time
        self.sampler = sampler
        self.tokenizer = tokenizer
        self.force_token = force_token
        self.start = tuple(tokenizer.think_start_tokens)
        self.end = tuple(tokenizer.think_end_tokens)
        self.thinking = initial_state == "reasoning"
        self.started = None  # Prefill/model loading is not thinking time.
        self.clock = time.monotonic
        self.max_bytes = limits["max_bytes"]
        self.max_seconds = limits["max_seconds"]
        self.max_tokens = max(1, max_tokens // 2)  # Reserve output space for the action/answer.
        self.bytes = self.tokens = 0
        self.history = []
        self.closing = []
        self.redirected = False

    def __call__(self, logprobs):
        if self.thinking and not self.closing:
            repeated = False
            # Four adjacent copies, at least 32 tokens: punctuation alone is not a loop.
            for width in range(8, min(128, len(self.history) // 4) + 1):
                block = self.history[-width:]
                if self.history[-4 * width:] == block * 4:
                    repeated = True
                    break
            timed_out = (self.max_seconds > 0 and self.started is not None
                         and self.clock() - self.started >= self.max_seconds)
            if self.redirected or repeated or timed_out or self.bytes >= self.max_bytes or self.tokens >= self.max_tokens:
                self.closing = list(self.end)
                self.redirected = True
                logging.info("Closing repetitive or over-budget thinking; continuing the completion")

        if self.closing:
            token = self.closing.pop(0)
            sampled = self.force_token(logprobs, token)
            observe = getattr(self.sampler, "observe", None)
            if observe is not None:
                observe(token)  # Inner samplers track committed tokens.
        else:
            sampled = self.sampler(logprobs)
            token = int(sampled.item())
        self.history.append(token)
        self.history = self.history[-max(512, len(self.start), len(self.end)):]
        if tuple(self.history[-len(self.end):]) == self.end:
            self.thinking = False
            self.started = None
            self.history.clear()
        elif not self.thinking and tuple(self.history[-len(self.start):]) == self.start:
            self.thinking = True
            self.started = None
            self.history.clear()
        elif self.thinking:
            if self.started is None:
                # The first sampled token has completed; pending GPU prefill cannot consume this
                # deadline while sampled.item() waits for it.
                self.started = self.clock()
            self.tokens += 1
            # Individual pieces conservatively overcount incomplete UTF-8, leaving headroom before
            # the client ceiling without decoding the growing prompt/history on each token.
            self.bytes += len(self.tokenizer.decode([token]).encode("utf-8"))
        return sampled


def install_thinking_control(server):
    """Request-local control for pinned MLX-LM, including its batched decoder path."""
    import math
    from contextvars import ContextVar
    if getattr(server, "_dial_thinking_installed", False):
        return
    context = ContextVar("dial_thinking_limits", default=None)
    original_handle = server.APIHandler.handle_completion
    original_generate = server.ResponseGenerator.generate
    original_tokenize = server.ResponseGenerator._tokenize
    original_sampler = server._make_sampler

    def handle(handler, *args, **kwargs):
        limits = getattr(handler, "body", {}).get("dial_reasoning")
        if not (isinstance(limits, dict)
                and type(limits.get("max_bytes")) is int and limits["max_bytes"] > 0
                and type(limits.get("max_seconds")) in (int, float)
                and math.isfinite(limits["max_seconds"]) and limits["max_seconds"] >= 0):
            limits = None
        token = context.set(dict(limits) if limits else None)
        try:
            return original_handle(handler, *args, **kwargs)
        finally:
            context.reset(token)

    def generate(generator, request, args, progress_callback=None):
        # The GPU queue crosses threads; attach only these request-owned values to its arguments.
        args._dial_thinking_limits = context.get()
        return original_generate(generator, request, args, progress_callback=progress_callback)

    def tokenize(generator, tokenizer, request, args):
        result = original_tokenize(generator, tokenizer, request, args)
        args._dial_initial_state = result[3]
        # Speculative/distributed samplers do not necessarily observe only committed tokens.
        provider = generator.model_provider
        args._dial_thinking_supported = (getattr(provider, "draft_model", None) is None
                                        and not getattr(provider, "is_distributed", False))
        return result

    def sampler(args, tokenizer):
        base = original_sampler(args, tokenizer)
        limits = getattr(args, "_dial_thinking_limits", None)
        if not (limits and getattr(args, "_dial_thinking_supported", False)
                and tokenizer.has_thinking and tokenizer.think_start_tokens and tokenizer.think_end_tokens):
            return base
        return _ThinkingSampler(base, tokenizer, limits, args._dial_initial_state, args.max_tokens,
            lambda scores, token: server.mx.full(scores.shape[:-1], token, dtype=server.mx.uint32))

    server.APIHandler.handle_completion = handle
    server.ResponseGenerator.generate = generate
    server.ResponseGenerator._tokenize = tokenize
    server._make_sampler = sampler
    server._dial_thinking_installed = True


def tool_grammar_enabled(environ):
    """Grammar-constrained tool calls are on unless the Rust supervisor sets DIAL_TOOL_GRAMMAR=0."""
    return environ.get("DIAL_TOOL_GRAMMAR", "1") != "0"


def install_tool_grammar(server):
    """Schema-constrain tool-call spans; must wrap the sampler before thinking control does."""
    import dial_tool_grammar as grammar
    if getattr(server, "_dial_grammar_installed", False):
        return
    original_tokenize = server.ResponseGenerator._tokenize
    original_sampler = server._make_sampler
    ops = grammar.MlxOps(server.mx)
    token_bytes = {}

    def tokenize(generator, tokenizer, request, args):
        result = original_tokenize(generator, tokenizer, request, args)
        provider = generator.model_provider
        # Speculative/distributed samplers do not necessarily observe only committed tokens.
        commits = (getattr(provider, "draft_model", None) is None
                   and not getattr(provider, "is_distributed", False))
        args._dial_tool_schemas = (grammar.tool_schemas(getattr(request, "tools", None))
                                   if commits and grammar.supported(tokenizer) else {})
        args._dial_grammar_state = result[3]
        return result

    def sampler(args, tokenizer):
        base = original_sampler(args, tokenizer)
        schemas = getattr(args, "_dial_tool_schemas", None)
        if not schemas:
            return base
        key = id(tokenizer)
        if key not in token_bytes or token_bytes[key][0] is not tokenizer:
            token_bytes.clear()  # One served tokenizer at a time; keep the table bounded.
            token_bytes[key] = (tokenizer, grammar.TokenBytes(tokenizer))
        return grammar.ToolCallSampler(base, tokenizer, schemas, args._dial_grammar_state, ops,
                                       token_bytes[key][1])

    server.ResponseGenerator._tokenize = tokenize
    server._make_sampler = sampler
    server._dial_grammar_installed = True


class ToolCallFormatter:
    def __init__(self, tool_parser, tools, streaming=False):
        self._idx = 0
        self._tool_parser = tool_parser
        self._tools = tools
        self._streaming = streaming

    def _format(self, parsed):
        name = parsed.get("name") if isinstance(parsed, dict) else None
        arguments = parsed.get("arguments") if isinstance(parsed, dict) else None
        valid = isinstance(name, str) and bool(name) and isinstance(arguments, dict)
        if not valid:
            logging.warning("Model emitted an invalid tool-call structure; returning a non-executable call")
        # The Rust client marks JSON null arguments malformed. {} would incorrectly turn a missing
        # argument object into a valid call to a zero-argument tool.
        call = {
            "id": str(uuid.uuid4()),
            "type": "function",
            "function": {
                "name": name if isinstance(name, str) else "",
                "arguments": json.dumps(arguments if valid else None, ensure_ascii=False),
            },
        }
        if self._streaming:
            call["index"] = self._idx
            self._idx += 1
        return call

    def __call__(self, tool_calls):
        result = []
        for text in tool_calls or []:
            try:
                parsed = self._tool_parser(text, self._tools)
            except (ValueError, TypeError, KeyError, IndexError, AttributeError):
                # Invalid model syntax is data, not a server exception. Backend/infrastructure errors
                # outside parsing still propagate rather than being mislabelled as model mistakes.
                parsed = None
            calls = parsed if isinstance(parsed, list) and parsed else [parsed]
            result.extend(self._format(call) for call in calls)
        return result

def _progress_packet(phase, **values):
    return {"dial_progress": {"phase": phase, **{k: v for k, v in values.items() if v is not None}}}


def _observed_tokens(iterator, publish, prompt_tokens=None):
    """Count engine tokens, including hidden reasoning/tool markup and batched vision tokens."""
    import math
    import time
    count = 0
    first = None
    last_sent = 0.0
    try:
        for item in iterator:
            cumulative = getattr(item, "generation_tokens", None)
            count = int(cumulative) if cumulative is not None else count + int(getattr(item, "token_count", 1))
            now = time.perf_counter()
            if first is None:
                first = (now, count)
            elapsed = now - first[0]
            rate = (count - first[1]) / elapsed if elapsed > 0 and count > first[1] else None
            reported_rate = getattr(item, "generation_tps", None)
            if reported_rate is not None and math.isfinite(reported_rate) and reported_rate > 0:
                rate = reported_rate
            if now - last_sent >= 0.08 or getattr(item, "finish_reason", None):
                publish(_progress_packet("generating", output_tokens=count,
                    prompt_tokens=prompt_tokens or getattr(item, "prompt_tokens", None), tokens_per_second=rate))
                last_sent = now
            yield item
        if first is not None:
            publish(_progress_packet("generating", output_tokens=count, prompt_tokens=prompt_tokens,
                tokens_per_second=rate))
    finally:
        close = getattr(iterator, "close", None)
        if close is not None:
            close()


def install_text_progress(server):
    """Instrument the pinned MLX-LM HTTP boundary without changing decoding or tool parsing."""
    from contextvars import ContextVar
    if getattr(server, "_dial_progress_installed", False):
        return
    handler_context = ContextVar("dial_text_progress_handler", default=None)
    original_handle = server.APIHandler.handle_completion
    original_generate = server.ResponseGenerator.generate

    def handle(handler, *args, **kwargs):
        token = handler_context.set(handler if handler.stream else None)
        try:
            return original_handle(handler, *args, **kwargs)
        finally:
            handler_context.reset(token)

    def generate(generator, request, args, progress_callback=None):
        handler = handler_context.get()
        if handler is None:
            return original_generate(generator, request, args, progress_callback=progress_callback)

        def publish(packet):
            handler.wfile.write(f"data: {json.dumps(packet, allow_nan=False)}\n\n".encode())
            handler.wfile.flush()

        ctx = None
        def progress(processed, total):
            if progress_callback is not None:
                progress_callback(processed, total)
            cached = max(0, ctx.prompt_cache_count) if ctx is not None else 0
            publish(_progress_packet("processing", processed_tokens=cached + processed,
                prompt_tokens=len(ctx.prompt) if ctx is not None else total))

        ctx, response = original_generate(generator, request, args, progress_callback=progress)
        def observed():
            # Iteration starts after the handler sends SSE headers.
            publish(_progress_packet("processing", prompt_tokens=len(ctx.prompt),
                processed_tokens=max(0, ctx.prompt_cache_count)))
            yield from _observed_tokens(response, publish, len(ctx.prompt))
        return ctx, observed()

    server.APIHandler.handle_completion = handle
    server.ResponseGenerator.generate = generate
    server._dial_progress_installed = True


class _VisionProgressState:
    def __init__(self):
        import threading
        self.lock = threading.Lock()
        self.latest = _progress_packet("processing")
        self.version = 1

    def publish(self, packet):
        with self.lock:
            self.latest = packet
            self.version += 1

    def snapshot(self):
        with self.lock:
            return self.version, self.latest


class VisionProgressMiddleware:
    """Request-isolated telemetry while the vision runner prepares media or decodes hidden tokens."""
    context = None

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        import asyncio
        from contextlib import suppress
        if scope["type"] != "http" or scope.get("path") not in ("/v1/chat/completions", "/chat/completions"):
            return await self.app(scope, receive, send)
        state = _VisionProgressState()
        token = self.context.set(state)
        lock = asyncio.Lock()
        task = None
        streamed = False
        finished = False
        seen = 0

        async def progress():
            nonlocal seen
            version, packet = state.snapshot()
            if version != seen:
                await send({"type": "http.response.body", "body":
                    f"data: {json.dumps(packet, allow_nan=False)}\n\n".encode(), "more_body": True})
                seen = version

        async def monitor():
            while not finished:
                async with lock:
                    if not finished:
                        await progress()
                await asyncio.sleep(0.08)

        async def observed_send(message):
            nonlocal streamed, task, finished
            async with lock:
                if message["type"] == "http.response.start":
                    streamed = any(k.lower() == b"content-type" and b"text/event-stream" in v.lower()
                        for k, v in message.get("headers", []))
                    await send(message)
                    if streamed:
                        await progress()
                        task = asyncio.create_task(monitor())
                else:
                    if streamed and message["type"] == "http.response.body":
                        await progress()
                        if not message.get("more_body", False):
                            finished = True
                    await send(message)
        try:
            await self.app(scope, receive, observed_send)
        finally:
            finished = True
            if task is not None:
                task.cancel()
                with suppress(asyncio.CancelledError):
                    await task
            self.context.reset(token)


def install_vision_progress(server):
    from contextvars import ContextVar
    if not hasattr(server, "ResponseGenerator") or not hasattr(server, "app"):
        return
    if getattr(server, "_dial_progress_installed", False):
        return
    VisionProgressMiddleware.context = ContextVar("dial_vision_progress", default=None)
    original = server.ResponseGenerator.generate
    def generate(generator, *args, **kwargs):
        state = VisionProgressMiddleware.context.get()
        ctx, iterator = original(generator, *args, **kwargs)
        if state is None:
            return ctx, iterator
        state.publish(_progress_packet("processing", prompt_tokens=ctx.prompt_tokens))
        return ctx, _observed_tokens(iterator, state.publish, ctx.prompt_tokens)
    server.ResponseGenerator.generate = generate
    # The non-batching fallback is called in worker threads with the request context copied by to_thread.
    import importlib
    openai = importlib.import_module("mlx_vlm.server.openai")
    original_stream = openai.stream_generate
    def stream(*args, **kwargs):
        iterator = original_stream(*args, **kwargs)
        state = VisionProgressMiddleware.context.get()
        return iterator if state is None else _observed_tokens(iterator, state.publish)
    openai.stream_generate = stream
    server.app.add_middleware(VisionProgressMiddleware)
    server._dial_progress_installed = True


def watch_parent():
    # A hard-killed Dial cannot run Rust Drop. Release the model even in that case.
    import os
    import threading
    import time
    parent = int(os.environ.get("DIAL_PARENT_PID", "0"))
    if parent > 1:
        def watch_parent():
            while True:
                if os.getppid() != parent:
                    os._exit(0)
                time.sleep(1)
        threading.Thread(target=watch_parent, daemon=True).start()


def main():
    watch_parent()
    import os
    if os.environ.get("DIAL_MODEL_ENGINE") == "mlx-vlm":
        try:
            from mlx_vlm import server
        except ModuleNotFoundError as error:
            if error.name == "mlx_vlm":
                raise RuntimeError("Local vision requires MLX-VLM in Dial's Python runtime; run dial setup") from error
            raise
        install_vision_progress(server)
        from dial_steering import install_vision_server
        specification = os.environ.get("DIAL_STEERING_SPEC", "")
        install_vision_server(server, json.loads(specification) if specification else None)
        server.main()
        return
    from mlx_lm import server
    # The pinned server has no formatter factory parameter; replace this single protocol boundary.
    server.ToolCallFormatter = ToolCallFormatter
    if tool_grammar_enabled(os.environ):
        install_tool_grammar(server)
    install_thinking_control(server)
    install_text_progress(server)
    from dial_steering import install_server
    specification=os.environ.get("DIAL_STEERING_SPEC", "")
    install_server(server, json.loads(specification) if specification else None)
    import sys
    if any(arg.startswith("--draft-model") for arg in sys.argv[1:]):
        print("dial: speculative decoding enabled; tool grammar and thinking control are disabled for this server", file=sys.stderr, flush=True)
    server.main()


if __name__ == "__main__":
    main()
