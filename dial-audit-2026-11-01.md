# Dial repository audit — 2026-11-01

Scope: the whole `KiidxAtlas/dial` workspace at `21c0022` (v0.2.9). That is 9 crates plus the root package, about 482k lines of production Rust and 247k lines of tests, plus CI, release, packaging and docs.
Method: six parallel read-only reviews, one per area, plus the build, lint and test gates. Every High and Medium-High finding was re-read in the source before it went into this report; those are marked **[verified]**. The remaining findings come from the area reviews, each backed by a file:line reference, and were not independently re-checked. No exploit was actually executed; the scenarios were traced through the code.

---

## 1. Overall verdict

**The codebase is in good health, and defensive engineering is clearly a priority.** Build, format and lint are clean. Of 8,237 tests, 8,224 pass in a suitable environment, and every failure I could explain came from the environment. Panic discipline is unusually good for 482k lines (about 200 production `unwrap()` calls), there are only 6 TODO markers, SQL is parameterized throughout, and file I/O uses no-follow `openat` walks. Releases are signed and every CI action is pinned to a SHA.

**The risk is concentrated at a few trust boundaries where two well-built layers meet.** Three of these let a prompt-injected model, or a view-only collab guest, get more power than intended. Each fix is small and local. After those, the biggest long-term issues are maintainability (several 900–1,900-line functions) and release-pipeline hardening.

| Area | Grade | One-line summary |
|---|---|---|
| Correctness / panics | A- | No panics reachable from untrusted input were found. Two config-driven overflow panics. |
| Security boundaries | B | Strong layers, but 3 boundary gaps (§3.1–3.3) and several medium ones. |
| Concurrency / processes | B+ | Careful lock and reaping logic. A PID-reuse window and a session-claim race remain. |
| Persistence | B | Atomic writes and WAL are good. No SQLite schema versioning or retention for several stores. |
| Maintainability | C+ | Many huge functions, and duplicated pipelines on security paths. |
| Tests | B+ | Large suite and a real architecture guard, but many timing-sensitive tests and poor diagnostics when prerequisites are missing. |
| CI / release / supply chain | B | Pinned and locked builds, but the release gate and signing key are under-protected, and macOS and Windows get little lint coverage. |
| Docs / process | B- | Rich docs with some drift. Very large, vaguely described commits pushed straight to `main`. |

---

## 2. Gate results (commands actually run)

| Command | Result |
|---|---|
| `cargo fmt --check` | ✅ pass |
| `cargo build --locked --workspace --all-targets` | ✅ pass (5m47s, 4 cores) |
| `make lint` (`clippy --workspace --all-targets -D warnings`) | ✅ pass |
| `make test-rust` as **root**, without bubblewrap | ❌ 26 integration failures; the crate unit tests never ran (see note) |
| Integration binary as an unprivileged user with bubblewrap installed | 344 / 345 pass. The one failure needs a system D-Bus, which this container doesn't have. |
| Every crate's unit-test binary as an unprivileged user | 7,880 / 7,892 pass. All 12 failures come from the environment (below). |

Classifying the failures:

- **25 integration failures:** Dial's sandbox refuses to run without bubblewrap, and refuses to run as root. Both are by design. All 25 pass once bubblewrap is installed and the tests run as a normal user.
- **`cli_print::ordinary_print_turn_keeps_disabled_learning…`:** the container has no system D-Bus, so the "could not prevent sleep" notice appears, and the test rejects any notice it didn't expect.
- **5 `workers::isolation` tests plus 2 `steering::routing` tests:** `/dev/fuse` is root-only here and kernel overlay mounts are refused. The routing tests very likely fail for the same overlay reason, but I did not prove it. All 7 pass as root.
- **3 more:** artefacts of my test setup — `cargo` missing from the test user's PATH, `SSL_CERT_FILE` pointing into `/root`, and the rar fixtures needing write access to the checkout. They pass when run as root.

Optional tiers not run: `test-mlx`, `test-smoke`, `test-drivers`, `test-python`, `make preflight`, and any macOS or Windows builds.

**Test-practice findings from this run:**

- `make test-rust` doesn't pass `--no-fail-fast`, so one failing binary hides every other crate's results.
- A missing sandbox prerequisite shows up as 25 unrelated-looking assertion failures (for example `assertion failed: e.project.path().join("one").is_file()`), not as a single "sandbox unavailable" skip or failure.
- `cli_print` depends on whether the host has a D-Bus.

---

## 3. Findings, ranked

### HIGH

#### 3.1 A glob bypasses the "safe bash" allowlist: no prompt, no checkpoint **[verified]**
`crates/dial-agent/src/agent/safety/approval.rs:18-56` (`shell_words`) refuses `$ ` ( ) { } < > & ; |`, but it accepts unquoted `*`, `?` and `[`. `safe_bash` (`:313-401`) then checks the *unexpanded* words for `find -delete`, `sed -i` and `tree -o`. The command later runs through `bash -c` with globbing enabled. `safe_bash` passing means `is_read_only` returns true (no prompt) and `opaque_shell_effect` returns false (no checkpoint).

- **Scenario:** a hostile repo ships a file literally named `-delete`, or the model creates one with `write`, which needs no prompt in the default tier. Then `find . -delet?` expands to `find . -delete`, and the tree, `.git` included, is deleted silently with no checkpoint. `sed -n 1p [-]i f` and `tree [-]o out` work the same way.
- **Fix:** in `shell_words`, return `None` for unquoted `*`, `?` and `[` (and `~`). Add tests for the three cases above. Longer term, replace the five separate shell splitters in `approval.rs` with one lexer that understands quoting and globs.

#### 3.2 Native file tools can write `.git/config` and hooks, and host-side git then runs them outside the sandbox **[verified]**
The sandbox makes `.git` read-only for `bash` (`crates/dial-tools/src/tools/exec/sandbox.rs:13-17` says why: hooks and config "are executed later by unsandboxed git"). However, `write`, `edit` and `ast_edit` resolve paths through `Workspace::resolve` (`crates/dial-tools/src/tools/mod.rs:906-1024`), which never refuses `.git/**`. These tools are auto-approved under the default `Approval::Write` tier (`approval.rs:1297-1301`). The git tool's read actions run host `git` with no approval (`crates/dial-tools/src/tools/vcs/git.rs:53-62`). Its `-c` overrides (`:160-176`) turn off fsmonitor, the pager, `diff.external` and hooks, but not `filter.<x>.clean/process`. `git status` and `git diff` run those filters on modified tracked files.

- **Scenario:** a prompt-injected model writes `[filter "x"] clean = sh -c '…'` into `.git/config`, writes `* filter=x` into `.gitattributes`, touches a tracked file, and calls `git status`. Arbitrary code then runs as the user, unsandboxed, with no prompt at any step. A planted `.git/hooks/pre-commit` also runs on any approved commit.
- **Fix:** refuse mutations under `sandbox::git_metadata(roots)` inside `Workspace::resolve` for write and edit callers, as `is_protected` already does for Dial storage. Separately, run git-tool reads inside the sandbox, or require approval when repo config defines `filter.*`, `core.hooksPath` and similar keys.

#### 3.3 A view-only collab guest can take over the host role and capture the write token **[verified]**
View links and full links share one room key (`crates/dial-app/src/collab/protocol.rs:98-122`). The sender's role is a self-declared string sealed under that key (`collab/socket.rs:817-842`). The relay gives the host slot to whoever connects first with `role=host` (`collab/server.rs:208-215`). The guest `hello` frame carries the raw write token (`protocol.rs:239-243`) and is re-sent on every reconnect.

- **Scenario:** the relay restarts or the host's connection drops. A view-link holder reconnects as host first. Writable guests reconnect and send `hello{writeToken}`, which the impostor decrypts. The impostor can now forge the transcript for everyone, and can join the real host as a writable guest, which allows steering if `allow_guest_steering` is on.
- **Fix:** never send the token. Use HMAC challenge-response over a nonce from the host. Sign host frames with a key whose public half is in the link, or derive a separate guest→host key from the write token.

### MEDIUM-HIGH

#### 3.4 Release gate and signing key **[verified]**
`.github/workflows/release.yml:202-226` accepts *any* successful `ci.yml` run on the tagged SHA, including `pull_request` runs. So a commit that never landed on `main` can be tagged and published, even though the error text says otherwise. The publish job has no `environment:` with required reviewers. `DIAL_RELEASE_SIGNING_KEY`, which `dial update` trusts (`crates/dial-app/src/release.rs:15`), is written to `$RUNNER_TEMP/release-key.pem` and never deleted. The same job then runs Python scripts and Docker builds with `contents`, `packages` and `pull-requests` write permission.

- **Fix:** require `git merge-base --is-ancestor $GITHUB_SHA origin/main`. Move signing into its own job behind a protected environment. `shred` the key file. Also: `${{ github.ref_name }}` is interpolated directly into PowerShell `run:` steps (`release.yml:173,176`); pass it through `env:` as the rest of the file does.

#### 3.5 Session writer claim races with replace and delete **[verified]**
`crates/dial-storage/src/storage/session.rs:1172-1203` checks the opened inode, then retries the lock for up to 2 s (`claim_writer`, `:43-56`). Once it holds the lock, it never re-checks that the path still names that inode. Separately, `remove_file` (`:1574-1585`) drops the lock *before* unlinking.

- **Scenario:** process A rewrites the session with `set_additional_dirs`, which renames over the path. Process B, waiting on the old inode's lock, then wins it. Both now think they own the session, and B's appends go to a deleted file and are lost.
- **Fix:** after the lock succeeds, compare `path_identity(path)` with the file handle and retry on mismatch. Unlink while still holding the lock.

#### 3.6 Text-markup tool calls are lifted on hosted endpoints, without checking the tool name **[verified]**
`crates/dial-llm/src/llm/mod.rs:837-853` runs `text_tool_calls::apply` on every endpoint. `apply` (`text_tool_calls.rs:38-48`) turns `<tool_call>` markup, or *any fenced JSON block* with exactly the keys `name` and `arguments`, into an executable `ToolCall`. Only the pythonic and reasoning lifts are limited to local endpoints and offered tools.

- **Scenario:** a model summarizing a hostile web page quotes a `{"name":"bash","arguments":{…}}` block. It becomes a real call, gated only by approval policy. Combined with 3.1 or the auto-approved tiers, that is enough.
- **Fix:** drop lifted calls whose names weren't offered. Lift fenced JSON only when it is the entire reply. Limit lifting to endpoints known to need it. Pass `text_calls_lifted` to approval as a lower-trust signal.

#### 3.7 Anthropic subscription path imitates Claude Code's client identity (policy risk)
`crates/dial-llm/src/llm/anthropic.rs:110-135` and `provider.rs:2952-3083` send `User-Agent: claude-cli/…`, `x-app: cli`, the Claude Code beta flag, and a computed `cch=` billing attestation with a hard-coded xxhash seed. The Codex WebSocket path hard-codes a client version (`codex_ws.rs`, around line 806).

- **Risk:** this isn't a code defect. It risks users' accounts under provider terms, and breaks silently whenever upstream changes.
- **Recommendation:** the owner should make an explicit decision. At minimum, put it behind a clearly labelled opt-in that documents the risk, and keep all pinned values in one place.

### MEDIUM

| # | Finding | Location | Fix |
|---|---|---|---|
| 3.8 | `ssh://` remote quoting uses POSIX `'\''` but is parsed by the remote *login* shell. Under fish, the path `/x';id;'` runs `id` (confirmed with a quoting simulator), so an approved "read" executes remote commands. | `dial-tools/.../integrations/ssh/connection.rs:133-135`, `ssh/url.rs:258-263` | Send the script as `echo <b64> \| base64 -d \| sh`, or refuse `'` and `\` in paths. |
| 3.9 | Contract back-compat for *registry* tools tries only 3 projections, not the powerset used for native tools. Sessions recorded at intermediate releases are refused on resume (`browser` has 49 additive properties). | `dial-tools/src/tools/surface/contracts.rs:366-428` | Generate candidates from release-ordered groups; add a test per intermediate shape. |
| 3.10 | Config-driven overflow panics: `Instant::now() + Duration::from_secs(timeout_secs)` with an unbounded `[laya] timeout_secs`; telemetry `flush_interval_secs` has no upper bound, and a panic there kills the hub thread, silently disabling OTLP and stats. | `dial-agent/src/agent/model/laya.rs:433-438`, `telemetry.rs:900,1323` | `checked_add` (as `compaction_runtime.rs:81` already does) plus upper bounds in validation. |
| 3.11 | Retry waits `max(hint, 300 s)` with no jitter. A `Retry-After: 1` still waits 5 min, mid-stream drops wait 300 s, and 10 attempts can stall a turn for about 45 min. Throttled subagents all retry together. (This is documented.) | `dial-llm/src/llm/retry.rs:344-352`, `provider.rs:39` | Honour the hint (capped), retry stream drops quickly, add jitter or exponential backoff. |
| 3.12 | The collab relay can be knocked over: unlimited threads per connection, unlimited rooms, and a `spawn()?` failure exits `serve` entirely. `--bind 0.0.0.0` is accepted with no warning. | `dial-app/src/collab/server.rs:50-66` | Log and continue on spawn failure, cap connections and rooms, warn on non-loopback binds. |
| 3.13 | The TUI doesn't restore the terminal on SIGTERM, SIGHUP or SIGQUIT (it only has a panic hook). The shell is left in raw mode with keyboard flags still set, and the `/relay` child is orphaned. | `dial-ui/src/ui/term.rs:183-207` | Route termination signals into the UI loop so `leave()` runs. |
| 3.14 | The git panel runs `git status` and `git diff` on the UI thread on every tick and selection move (60 s timeout), which contradicts ARCHITECTURE.md. The UI freezes in large repos. | `dial-ui/src/ui/overlay/git/mod.rs:155-165`, `git_panel/git.rs:433` | Move it to a worker with a generation counter, as `file_discovery.rs` does. |
| 3.15 | A panic in the RPC/ACP actor ends the session silently: no `catch_unwind`, no reply to the in-flight id, and no `fatal` event. | `dial-app/src/rpc.rs:1008,1058,1537`, `rpc/actor.rs:772` | Wrap each request's dispatch, then emit an error and a `fatal` event. |
| 3.16 | `ProcessGroup.child` is public. Callers reap the leader directly, and `Drop` then calls `killpg` on a PID that may have been reused. | `dial-core/src/runtime/process.rs:10-46`; callers in `loop_runner.rs:326`, `app/git.rs:268`, `schedule.rs:1380` | Make `child` private and expose `wait`/`try_wait` that mark the group terminated. |
| 3.17 | The Linux launcher thread (new in `21c0022`) lives in a `OnceLock`. If it dies, every later spawn fails. Failed send/recv paths lose the `Command` (`mem::replace`), so temp scripts leak. Fork safety itself is fine. | `dial-core/src/platform/unix/process.rs:38-82` | `catch_unwind` around `spawn()` inside the loop, and return the `Command` on error. |
| 3.18 | Linux desktop apps launched by the agent inherit Dial's full environment (provider API keys) and its process group. | `dial-computer/.../linux/applications.rs:654-676` | `process_group(0)` or `setsid`, plus a cleaned environment. |
| 3.19 | The computer tool's Node worker gets the full environment and `require`. `read_only` only disables helper functions. Each run needs approval. | `dial-computer/src/computer/supervisor.rs:540-546`, `js/worker.mjs` | Clean the environment, and document `read_only` as advisory or enforce it with Node permissions. |
| 3.20 | Two macOS spawns bypass the `SPAWN` lock (the same pipe-inheritance class that `e1473f1` fixed). | `screen_capture_kit.rs:116`, `computer/rpc.rs:400` | Route through `platform::process::spawn`. |
| 3.21 | Worker approval is implemented twice (subagents and live tasks), plus the parent's own call sites. Copies of a security boundary will drift. | `dial-agent/.../subagents.rs:1503-1620`, `live_tasks.rs:1495-1580` | One `authorize_worker_call` in `safety/approval.rs`. |
| 3.22 | No SQLite schema versioning (`user_version`) in any store, and no `ALTER TABLE` anywhere. New columns never reach existing DBs, and old binaries open new DBs unchecked. | `dial-storage`: `store.rs:271`, `archive.rs:85`, `local.rs:83`, `semantic_store.rs:251`, … | Ordered migrations inside an IMMEDIATE transaction; refuse newer DBs. |
| 3.23 | CI lints only on Linux, so macOS/Windows code (including 245 `unsafe` blocks in dial-computer) is never linted. The Windows test suite is a 17-line allowlist. The `wayland-pipewire` feature is never compiled. | `.github/workflows/ci.yml:128-140`, `scripts/windows-contract.txt` | Add clippy jobs for macOS and Windows, plus a feature-powerset compile check. |
| 3.24 | `unsafe` documentation: 633 production `unsafe {}` blocks and 387 `// SAFETY:` comments, with no lint enforcing them (dial-storage has 23 and 0, dial-core 231 and 79). Raw `openat`/`mkdirat` is duplicated outside `dial-core/platform`. | `Cargo.toml:135-139` `[workspace.lints]` | Enable `clippy::undocumented_unsafe_blocks`; move the raw libc calls into `platform`. |
| 3.25 | The secret scrubber for Dev-policy, unsandboxed and MCP children is a name denylist. It misses `*_DSN`, `SLACK_WEBHOOK_URL`, `*_JWT`, `OP_SESSION_*`, `STRIPE_SK_*`, and URL userinfo with no `:`. | `dial-tools/src/tools/exec/sandbox.rs:888-938` | Use an allowlist (PATH, HOME, LANG, toolchains, plus user-chosen extras). |

### LOW / hygiene

**Correctness and robustness**
- `LiveTasks::status` hashes up to 64 MiB per job while holding the global jobs lock. A timeout turns an accepted receipt into `NeedsVerification`, so a read changes state (`live_tasks.rs:280`, `verify/worker_acceptance.rs:60-73`).
- Inconsistent poisoned-lock handling: three `.expect("…poisoned")` calls next to `into_inner()` recovery (`live_tasks.rs:142,775,1337`).
- `ProcId::signal_group` checks start time and then calls `killpg` in two separate steps (a race window). `same_process` doesn't reject a deserialized pid ≤ 1 (`dial-core/src/platform/procinfo.rs:49-51`).
- The SSE parser splits only on `\n` (a server using bare `\r` builds one huge line until the 1 MiB cap) (`dial-llm/src/llm/stream.rs:228`).
- Bedrock region detection uses a fixed prefix list (missing `cn-` and `eusc-`) and mis-signs requests through corporate gateways (`bedrock.rs:36-55`).
- `remove_tree` unlinks while iterating the same directory stream, which can skip entries on macOS (`memory_backend/files.rs:302-345`).
- The lease heartbeat treats `SQLITE_BUSY` as a lost lease (`local.rs:52`).
- The session search index skips the owner-only open, ignores `chmod` errors, and resumes from a byte offset that header rewrites invalidate (`session.rs:4140-4240`).
- `/relay start` reports "Listening" before the child has bound the port (`maintenance.rs:609-624`).
- Startup writes `\x1b[3J`, which erases the user's scrollback (`ui/term.rs:186`).
- The desktop input lock sits at a predictable `/tmp/dial-desktop-input-{uid}.lock`, so another local user can block it (`desktop/control.rs:567`).
- The macOS SkyLight code reads a private struct at guessed offsets, and `transmute_copy` has no size assertion (`skylight.rs:210-218,917-940`).
- `unsafe impl Send` relies on a private field of the `reis` crate (`libei.rs:70-77`).

**Security hygiene**
- `SecretToken::wipe` is a plain `fill(0)` that the compiler may elide; use `zeroize` (`auth.rs:41-45`).
- `config::secrets::Config` derives `Debug` over literal secrets (`config/secrets.rs:9`).
- "Forget" in the semantic store doesn't set `secure_delete`, so the text survives in free pages and the WAL (`semantic_store.rs:220-244`).
- Path deny rules don't apply to `bash` or `grep`. This is documented, but `cat key.pem` gets past a `deny read *.pem` rule with no prompt. Surface this in the rule UI.
- Contract hashes rely on serde_json's `preserve_order` staying off. A future dependency enabling it changes every identity and breaks additive stripping (`contracts.rs:734-740`). Sort keys explicitly.
- `file_identity` re-opens the path instead of using the no-follow walk (`tools/mod.rs:1145-1157`).
- `docker-compose.yml` defaults to `seccomp=unconfined`, and `.env` is in neither `.gitignore` nor `.dockerignore`.

**Supply chain and CI**
- nextest is installed via `curl | tar` with no checksum, and pip installs are unpinned (`ci.yml:58,90,184`).
- There is no Dependabot or Renovate.
- Floating runner images.
- No build provenance, SBOM or image signing.
- `SOURCE_DATE_EPOCH` isn't set, so builds aren't reproducible.
- `latest` moves even when an older tag is pushed.
- 58 duplicated crate names in `Cargo.lock`, among them three `fancy-regex` versions and five `windows-sys`. `fs2` is unmaintained, and `std::fs::File::lock` replaces it. `deny.toml` targets omit aarch64-linux and musl. `MPL-2.0` is allowed despite the comment saying otherwise.
- `paths-ignore: '*.md'` skips CI for `CHANGELOG.md`, which is compiled into the binary (`dial-app/src/changelog.rs:7`).
- The flake has no `flake.lock`.
- `dial-build-info` watches `../../crates` and embeds `now()`. This probably forces large downstream rebuilds on any edit (inferred from the build script and dependency graph, not measured).

**Architecture guard gaps**
- The layer rule matches only `crate::` paths (a `dial_llm::…` import would slip through).
- `git_panel` sits beside `ui` and uses `tools::vcs`/`config::types` without being covered by the `ui` rule.
- AGENTS.md says the serde `default, deny_unknown_fields` rule is "enforced by tests", but no test checks it. Compliance is good anyway: 89 of 96 structs comply, and the other 7 are justified.

**Warnings suppressed against AGENTS.md:** `#[allow(unused_imports)] mod tests;` (`dial-tools/.../lsp/servers.rs:696`) and three `#[allow(dead_code)]` fields (`security/scan.rs:237-258`).

**Duplication** (against the "one owner" rule):
- Four hand-written OAuth loopback servers.
- A second retry loop in `usage_report/claude.rs`.
- Eight-plus hand-built `ureq` agents that each restate the redirect, proxy and timeout policy.
- About ten copies of a byte-boundary truncation helper in dial-agent.

---

## 4. Maintainability

These largest functions are hard to review, rustfmt silently gives up on some of them, and every one sits on a hot path:

| Function | Lines | Location |
|---|---|---|
| `turn_step` | 1,933 | `dial-agent/src/agent/run/turn/step.rs:7` |
| `run_inner` (+1,700-line closure rustfmt skips) | 1,825 | `dial-agent/src/agent/workers/child.rs:1897` |
| `dispatch_calls` | 1,229 | `dial-agent/src/agent/run/turn/round.rs:94` |
| `turn_inner` | 1,004 | `dial-agent/src/agent/run/turn_loop.rs:249` |
| `chat_with_control_events_inner` (retry, OAuth, failover) | 918 | `dial-llm/src/llm/provider.rs:1430` |
| `command` | 861 | `dial-app/src/app/shell/command.rs:97` |
| `call_tool_inner` | 734 | `dial-agent/src/agent/workers/child.rs:4185` |
| `run` (RPC actor) | 690 | `dial-app/src/rpc/actor.rs:86` |

In dial-agent alone, 36 functions exceed 200 lines. The largest files are `child.rs` (5,119 lines), `session.rs` (4,462), `settings.rs` (3,527), `subagents.rs` (3,376), `hashline.rs` (3,234), `catalog.rs` (3,200) and `provider.rs` (3,192).

**rustfmt gives false assurance here.** The `child.rs` closure that starts at `:2133` is unformatted, and some lines reach 478–509 characters, yet `cargo fmt --check` passes because rustfmt gives up silently. Extracting phase functions over a shared context struct fixes both problems. `round.rs`'s `native_edit_preflight` already shows the pattern.

---

## 5. Process and repository practices

- **Commits:** large, vaguely described commits go directly to `main` ("big update coming": +50k lines over 511 files; `d7b5917`: 56k lines over 483 files). This makes review and bisecting hard. Note that this clone is shallow, so the single 806k-line root commit is an artefact of the cut, not a squash.
- **Release cadence:** nine releases (0.2.1 → 0.2.9) in four days, five of them on 2026-10-28. Combined with 3.4, a release depends on very little besides a green run.
- **CHANGELOG:** the `[Unreleased]` section is 67 KB, with single lines over 1,100 characters. It's hard to use as release notes.
- **Docs drift:**
  - `README.md:73-75` documents `DIAL_REQUIRE_SIGNATURE`, which doesn't exist. The installer actually requires a signature by default and opts out with `DIAL_ALLOW_UNSIGNED_INSTALL`.
  - `docs/release-and-install.md:98` is stale about OpenSSL.
  - `ARCHITECTURE.md:117` omits `config::image`.
  - AGENTS.md's "sibling `tests.rs`" rule is looser in practice: 127 differently named test modules pass the guard.
- **Tests:**
  - 165 sleeps in 77 test files, and 128 assertions on elapsed time, some with tight limits (<100 ms, <160 ms), run in parallel with `retries = 0`. These are the likeliest sources of CI flakes.
  - 31 `#[ignore]`d tests, all with a reason.
  - No tests that hit real external networks (good).

---

## 6. Strengths worth keeping

- **Architecture is enforced by code, not just documented.** `tests/integration/architecture.rs` checks crate layers (including dev and target dependencies), module boundaries and file layout, and has fixture tests of its own. The crate graph matches ARCHITECTURE.md exactly.
- **Filesystem safety:**
  - `openat` with `O_NOFOLLOW` walks that re-check dev and inode.
  - Atomic publish (temp file, fsync, rename, then fsync of the directory).
  - Secrets files at 0600 and directories at 0700, with owner and link-count checks.
  - Private state kept outside the workspace.
- **Network safety:**
  - The SSRF guard runs inside the resolver, so DNS rebinding can't get around it.
  - It covers mapped, NAT64 and 6to4 addresses and the metadata IPs.
  - Redirects are followed one hop at a time, and auth headers stay with the original origin.
  - Response bodies are capped, and rustls has no verifier overrides.
- **No regex denial of service:** model-supplied regexes use the linear-time `regex` crate with size limits.
- **The sandbox fails closed.** It runs real enforcement probes, has no implicit unsandboxed fallback, and refuses to run as root. This audit's test run showed that behaviour working as designed.
- **Robust parsing:**
  - Bounded parsers with CRC and length checks for Bedrock event streams, Connect and GGUF.
  - UTF-8-safe streaming.
  - Partial tool calls never run.
- **Panic hygiene:** low `unwrap()` and `expect()` counts, and no indexing on untrusted data without a check.
- **Build discipline:**
  - Every action pinned to a SHA, `contents: read` by default, no `pull_request_target`.
  - `--locked` everywhere, cargo-deny, crates.io as the only dependency source, and every crate using `workspace = true` dependencies.
- **Signed releases:** Ed25519 plus sshsig signatures, an installer that refuses unsigned builds, and self-update that requires a compiled-in key.
- **Minimal suppressions:** about 50 `#[allow]`s in total, mostly `too_many_arguments` with a stated reason, and 6 TODO markers in 482k lines.

---

## 7. Suggested order of work

1. **This week (security):** 3.1 (reject unquoted globs), 3.2 (refuse writes under `.git` and sandbox the git-tool reads), 3.6 (validate lifted tool names), 3.4 (release gate on `main` plus a protected signing environment). Each is a small, local change with a regression test.
2. **Next:** 3.3 (collab token challenge and host signing), 3.5 (re-check the session claim identity), 3.10 (`checked_add`), 3.16 and 3.17 (process-group ownership), 3.25 (environment allowlist).
3. **Ongoing:**
   - Split the top 8 functions.
   - Unify the shell lexer and the worker approval path.
   - Enable `undocumented_unsafe_blocks`.
   - Add macOS and Windows clippy jobs.
   - Add SQLite `user_version` migrations.
   - Add `--no-fail-fast` and a clear sandbox-prerequisite check to `make test-rust`.
   - Add Dependabot.
4. **Decision needed by the owner:** 3.7 (imitating the Claude Code client identity).
