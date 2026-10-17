//! Fake `python -m mlx_lm.server` for the server tests of this crate and its dependents
//! (`test-support`).
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

/// Stands in for `python -m mlx_lm.server`. The `--model` value selects a
/// behaviour: `exit-early`, `never-ready`, `bad-model`, or a path that
/// receives `{"pid", "child", "argv"}` once the server is listening.
const FAKE_SERVER: &str = r#"#!/usr/bin/env python3
import http.server, json, os, socketserver, subprocess, sys
opts = dict(a.split("=", 1) for a in sys.argv[1:] if a.startswith("--") and "=" in a)
mode = opts["--model"]
if mode == "exit-early":
    sys.stderr.write("fatal: cannot load model\n")
    sys.exit(3)
if mode == "never-ready":
    import time
    time.sleep(300)
helper = subprocess.Popen(["sleep", "300"])
marker = os.path.join(mode, "info.json") if os.path.isdir(mode) else mode
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, code, payload):
        data = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        self.reply(200, {"object": "list", "data": []})
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if mode == "bad-model":
            self.reply(500, {"error": "adapter config is broken"})
        else:
            with open(marker) as f: info = json.load(f)
            info.setdefault("requests", []).append(body)
            with open(marker, "w") as f: json.dump(info, f)
            if os.path.basename(mode) == "slow-warmup":
                import time
                time.sleep(300)
                return
            content = body.get("messages", [{}])[-1].get("content")
            if content == "dial-test-fail":
                print("RuntimeError: test inference allocation failed", file=sys.stderr, flush=True)
                self.reply(500, {"error": "inference failed"})
                return
            if content == "dial-test-stall":
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(b'data: {"dial_progress":{"phase":"processing","prompt_tokens":42}}\n\n')
                self.wfile.flush()
                # Deliberately ignore a client disconnect, like an uninterruptible GPU prefill.
                import time
                time.sleep(300)
                return
            self.reply(200, {"choices": [{"finish_reason": "length", "message": {"role": "assistant", "content": "pong"}}],
                             "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
# A fixture has no public hostname. HTTPServer's default server_bind calls
# getfqdn(), which can hang on hosted macOS reverse-DNS resolution before serving.
class FixtureServer(http.server.ThreadingHTTPServer):
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name = "localhost"
        self.server_port = self.server_address[1]
server = FixtureServer((opts["--host"], int(opts["--port"])), Handler)
if mode != "bad-model":
    open(marker, "w").write(json.dumps({"pid": os.getpid(), "child": helper.pid, "argv": sys.argv[1:], "engine": os.environ.get("DIAL_MODEL_ENGINE"), "apc": os.environ.get("APC_ENABLED"), "apc_config": {key: os.environ.get(key) for key in ("APC_DISK_ENABLED", "APC_NUM_BLOCKS", "APC_MEMORY_MAX_GB", "APC_MEMORY_RESERVE_GB", "APC_CHECKPOINT_ENTRIES")}}))
server.serve_forever()
"#;

pub fn fake_python(dir: &Path) -> PathBuf {
    let path = dir.join("fake-python");
    fs::write(&path, FAKE_SERVER).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}
