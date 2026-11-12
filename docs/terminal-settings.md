# Terminal settings

Search configuration, choose storage scope and review setup presets before saving.

[Terminal overview](terminal-interface.md) · [Documentation index](README.md)

### Settings

`/settings` is the configuration hub, including persistent learning preferences. The categories
are a one-row tab strip at the top that scrolls to keep the active one visible. `Left`/`Right` change categories,
`Up`/`Down` move through settings, and `Tab`/`Shift-Tab` jump sections within a category. The
first category, **Common**, also shows the model, approval mode, sandbox mode, Enter-while-working
and notification rows under one heading; they stay in their own categories too, and editing either
copy edits the same setting. Type to search labels, descriptions, paths and goal-oriented synonyms
across categories. An exact path or label ranks first, whole-word matches rank above scattered
letters, weak scattered matches are dropped, and each result names its category at the right edge.
Tab counts show saved overrides, not running tasks.
[Settings layout](configuration.md#settings-layout) describes
the categories; Safety groups approval, sandbox, isolation, folders and rules, while Connections
groups provider and tool-server configuration. Approval and sandboxing remain independent controls.

`Enter` toggles a two-state switch or opens an editor. Auto settings offer explicit **Auto / On / Off**
choices. Choices save only when confirmed; `Esc` cancels. The permanent scope line identifies the
named project or all-project destination; `Ctrl-S` switches it. Success is acknowledged only after
the controller saves. `Delete` removes one override, revealing its inherited value rather than
resetting unrelated settings. Details explain effective source, precedence and restart requirements.

`Ctrl-O` shows **Changed only** across categories for the chosen scope. `Ctrl-Delete` reviews the
exact visible saved leaf overrides and their masked fallback values before removing them together.
Nothing is removed until the rendered confirmation is accepted. Changed scope/values, invalid
configuration and incomplete embedding pairs require a new review; hidden partner settings are
never silently added. A failed review stays open with an error. Very short terminals cannot confirm
an unreadable review.

While you search, the category tabs are not offered and `Tab` does nothing; `Esc` clears the search first.

The footer shows at most four keys: the main action, search or filter, `F1 keys` and the way out.
`F1` opens the panel's full key sheet, which lists every key the footer left out; `/models` works
the same way. While a panel is open, the terminal title leads with its name.

**Context & history** opens with a summary card: a gauge of the context used against the active
limit (marked *estimated* when it is), the point where compaction starts, and the read-only server
capacity and compaction reserve. A fact that is missing says why (`nothing sent yet`, `not reported
by the server`) or which setting supplies it. The saved defaults follow; compaction tuning is folded
under **Advanced**.

A zero or empty value says what it does, in the words of the setting's description: `0 (unlimited)`,
`0 (off)`, `not set (python3 from PATH)`. An empty value whose description states no meaning reads
`not set`.

Choice lists also accept typing to filter their labels and stored values. `Esc` clears that filter
before returning to settings; `Enter` confirms only a visible match. Clearing an override restores
its inherited value, rather than resetting unrelated settings.

Settings that name something Dial can list offer it instead of a typing field; `Ctrl-E` in any of
these choosers switches to typing the value, and every chooser still accepts values it does not list:

- **Model rows** (draft model, architecture model, main model, decision-memory override, image
  model) list discovered and downloaded models. A suggested local model that is not downloaded is
  marked `· download`: the first `Enter` asks, the second downloads it in the background and saves
  the setting only when the download succeeds. A failed or cancelled download (`Esc` closes the
  panel and cancels it) keeps the previous value; opening a picker never starts a download. The
  main model row assigns the model through the same path as `/models`.
- **Name rows** (provider, model profile) list the configured names.
- **Path rows** (Python, Node, worker, IDA, LSP programs) open a file browser starting in the
  project. `Enter` opens a folder or chooses the entry; typing filters the listing, or type a path
  (`~/`, absolute or relative) and press `Enter`. Hidden entries appear when the filter starts with `.`.
- **Known-value lists** (status line segments, compaction order, integrations, foreign rule
  sources, recall types, learn-from models) open a checklist: `Space` checks or unchecks,
  `Shift-Up`/`Shift-Down` reorders preference lists, and the last row adds another value. Extra
  writable folders add entries through the folder browser.
- **Tables and arrays** open one row per entry; complex rows show counts instead of serialized
  contents. `Enter` descends into a record and edits existing scalar fields as plain typed values,
  preserving siblings. `Delete` removes an entry; **Save** returns the changed collection to the
  controller. Credential-bearing collections also use this focused view: scalar values are
  conservatively masked, including nested headers/authentication. `Ctrl-R` explicitly reveals an
  input. `Ctrl-E` retains Advanced TOML editing for representations not covered by focused controls.
  **Add** asks for the entry name and a value type, then a plain scalar value or nested table/array.
  Cancelling creation leaves no partial entry. Required fields and validation still belong to the
  canonical configuration owner; Advanced TOML remains available for datetime and special records.

The shared registry covers persisted configuration, including provider definitions, model profiles,
presets, custom tools, MCP/LSP/debug adapters, hooks, verification, budgets, training and voice.
`/improve settings` is a focused learning-preferences view of the same owner; Learn remains the
place for running training/evaluation operations. Search by a descendant path such as
`providers.custom.headers` or `browser.allowed_hosts` to find the owning editor. Advanced inputs
accept TOML inline tables/arrays, not JSON; browser restrictions use
`{ allowed_hosts = ["example.com"] }`.
Structured editors start with only the selected storage layer, not inherited global values. Saving
replaces that layer's value; untouched inherited fields continue to merge normally. Hooks retain their
append-across-layers semantics. `Delete` removes the selected layer's override. The controller validates
the full configuration before writing; failures keep the editor and its draft open.

Credential-bearing collections and literal secret lists are masked while browsing and editing.
`Ctrl-R` deliberately reveals/hides a sensitive editor; opening another editor always restores masking.
Prefer environment-variable references to literal tokens. Runtime-only paths, derived context and
private account policy are not editable settings. The retired `ui.hyperlinks` compatibility key is not
offered, and reserved MCP switches are explicitly labeled as having no current runtime effect.

### Session controls and linked pages

`F10` (rebindable as `app.status.focus`) or `/controls` opens the keyboard-accessible session control
doorway. The compact composer strip reports planning, approval, **main** reasoning and assistance.
Unknown active facts remain unknown; saved defaults waiting for the safe turn boundary are not
labelled active. Opening controls preserves the composer draft. Linked model, provider, Safety and
context settings keep their existing validation and approval owners.
Settings rows compare saved values with the active app snapshot: **Active A → Saved B** and
**after this turn** (or **next request** when idle), with a pending-count header. Display preferences
are distinguished as applying on panel close; restart-required and mode-inactive learning settings
are not counted as pending adoption. Sensitive values remain hidden in both columns.

The Context-defaults destination groups context caps, output limits, history budgets and compaction.
Its header separates recorded active occupancy/limit and read-only loaded-server capacity from
saved defaults. Missing server evidence says unknown. Usage estimates are labelled estimates.

Models distinguishes the active model from the highlighted selection. Tuning a highlighted model
does not assign it to main or a helper role. Selecting a model applies the assignment directly;
there is no confirmation screen. Clearing a role still reviews its exact saved removals and
inheritance effects before publication. Local preparation saves only on success.
The role inspector groups model, declared reasoning override/inheritance and connection overrides.
Headers are a visible masked action, not only a shortcut. Models, Providers and model tuning keep
their named global/project destination visible. Controls shows the recorded reason for the main
route; provider-error fallback and context promotion are distinct from worker missing-assignment
defaults. Historical provenance that was not recorded stays unavailable.
Providers offers **Edit configuration / endpoint → Settings providers**. Tool servers offers
**Add custom** (`a`) and **Edit configuration** (`d` on an installed MCP row). These open the
same typed collection editors under Settings, not another configuration store. Back retains the
origin, and the provider endpoint is refreshed from saved configuration without testing it.

Settings → Memory brings backend, extraction/embedding model IDs, storage scope and prerequisite
readiness together. Its model rows open the existing exact role picker; linked navigation returns
to the retained Memory page. Choosing a model never enables a memory backend. A successful reviewed
background model preparation returns to its originating Settings page; failed preparation remains
inspectable and does not publish the reviewed settings.

`Alt-Left` returns through retained linked pages. Settings keeps search/category/selection and
in-progress editors while refreshing values; a refresh failure reports that last-good values are
still shown. Worker steering drafts settle before a sibling viewer is constructed.

Activity groups work by existing owners. Downloads uses the same session-held manager as Models;
closing the model browser does not cancel explicitly started downloads. The worker viewer's
Output/Inspector panes distinguish recorded routing and controls from current defaults. Historical
missing routing, permissions or budget facts remain unavailable, not reconstructed.
Attention includes observed worker requests/failures, failed processes, blocked todos, main compute
waits and retained last download/listing failures. A last failure is historical evidence, not a claim
that the operation is still failing. Opening its owner is inspection, never an automatic retry.
While a main approval or question owns input, `F6` opens a frozen read-only Attention snapshot,
including that pending prompt. `F6` or `Esc` returns to the unchanged original prompt; Enter,
approval letters and paste cannot answer it from the snapshot. Cancellation still cancels the
original prompt. Timed typed questions pause their deadline during inspection and resume with
the exact remaining time. Missing or last-observed snapshots are not an all-clear.

The tabs, scope and search stay in fixed header rows while browsing. Appearance samples never
expand above the options automatically: `Shift+F4` explicitly opens a wrapped, scrollable preview
(plain `F4` too once the provider-setup shortcut no longer uses it) and
returns to controls with the draft retained, at every terminal size. `Enter` confirms through the
normal settings owner; `Esc` discards. Samples cover status,
tool output, motion, transcript detail, worker cards and the session rail; they are not live animation
or service tests. The side pane has no separate Appearance preference.

The command palette offers an opt-in unified search for actions, settings, sessions and help.
Matching alone never executes, changes settings or starts model requests. Default action search
is unchanged. Each inbox notice offers Read and a category-local preference action; errors open
error preferences and completion notices open the general completion/question preference. Categories
without a separate supported preference use the general destination. Opening preferences never
marks a notice read or disables warnings.

### Local and hosted setup presets

`/setup presets` offers **Local only**, **Local with approved hosted assistance**, and **Hosted**,
then **This project** or **All projects** scope. You can also open a specific review with
`/setup presets local-only project`, `local-assisted`, or `hosted`. Nothing is saved until the
final confirmation. Choose the main model in `/models` first; assisted setup also requires an
explicitly assigned hosted coach in `/roles`. Setup never picks an unconfigured model, probes a
provider, downloads weights, or changes credentials.

The main model stays unchanged. Small/compaction, task, tester, memory and decision roles
use that model; the coach does too except in assisted setup. Local presets require an on-device
runtime or loopback endpoint, not merely a provider named “local” or a LAN address. They enable
the lean tool/context profile. The review shows detected physical RAM, context and role choices;
`dial models recommend` provides weight/KV memory-fit estimates rather than guessing from RAM alone.

All three presets clear automatic fallback chains, select local provenance memory (no embedding
or memory-model requests), and disable Laya, teacher assistance, distillation and automatic
escalation. Assisted setup enables the reviewed hosted coach and sets **Ask** approval, preserving
**Readonly** if selected. Approved coach questions disclose their supplied evidence to that provider.
The review lists these changes before saving. Sandbox and read-before-edit protections are unchanged.
Conflicting project/CLI overrides fail before saving; global defaults are also validated without
the current project's overrides. Presets are durable configuration bundles, **not a network firewall**:
explicit later model/profile choices, custom agents and network tools retain their own policies,
and other projects can override global defaults.

The preset picker can also capture a named setup using its explicit safe boolean allowlist. It does
not capture credentials, endpoints, model identities, machine paths or arbitrary configuration.
Saved named setups use the same scope selection, review and validation as built-in presets.

The local-model outcome checklist links runtime setup, Models and explicit provider connection tests.
Browsing the checklist starts no installation, download or probe. Cached results are labelled last
known; installed executables do not establish running-service health. Readiness beside memory/voice
controls describes known prerequisites, not successful microphone, playback or live inference tests.

Readiness uses distinct evidence levels: **Configured**, **Available**, **Text tested**, and
**Tool-compatible**. Cached model metadata and saved credentials are not inference success.
Unobserved or omitted endpoints remain unknown; changing an endpoint discards prior metadata
success and model counts. Text and synthetic tool-roundtrip checks are separate opt-in diagnostics
for the exact tested configuration, retained only for the session. Neither certifies coding quality.

Implementation and verification of the forty approved outcomes is tracked separately in the
[acceptance ledger](control-surface-acceptance.md); this page is not a claim of complete acceptance.

To create a theme, put a JSON file such as
`~/.dial/themes/custom.json` there; it may extend a built-in (`dark`, `light`, `mono`, `gruvbox`,
`nord`, or `paper`) and overrides reload live. `NO_COLOR` forces monochrome output.

`Ctrl-S` changes global/project storage scope in the model, provider and settings panels.
