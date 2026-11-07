# Models and roles

Keep model selection, roles, favorites, downloads and per-model tuning together in the Models panel.

[Terminal overview](terminal-interface.md) · [Documentation index](README.md)

- In `/models`, highlight a model and press `Ctrl+F` to favorite or unfavorite it.
  The **Favorites** view lists saved favorites; `*` marks favorites that are not the current main model.
  Favorites use the picker's global/project scope (`Ctrl-S`) and persist across restarts.
  In All models, favorites are pinned below the current main model and above other models.
  Press `F8`, `Option+]` on macOS or `Alt+]` elsewhere (remappable as `app.model.favoriteNext`), or use `/model next` to cycle through
  available chat favorites without opening the picker. Unavailable/embedding models are skipped;
  a switch requested during a turn saves immediately and is adopted when the turn finishes rather than interrupting it.
  On the first press, if discovery is needed, the switch is retained and applies when ready.
  Escape or `/model cancel` cancels that pending switch. Changes to the selected model, saved
  favorites, scope, or discovery request also cancel it rather than applying a stale choice.
- `Shift+F8` edits a model's HTTP headers for the role being picked (`Roles`, then `Enter`, then `Shift+F8`). The editor title names the model and role, and saving assigns only that role. Outside a role pick, `Shift+F8` asks you to choose a role first, so browsing never reassigns main. Inside the editor, `F9` inherits the provider's headers (`Tab` only moves between fields, so it works on legacy terminals). Below 56 columns the groups column is hidden and cannot take focus. The unmodified `F8` still works here when the global next-favorite action is not bound to it.
- Model lists in `/models` filter and sort without leaving the list. `Shift+F2` cycles the **show** filter (any, available, loaded, local, cloud, tools, vision, reasoning, priced, used) on top of the kind tabs and search; `Shift+F3` cycles the **sort** (default ranking, name, cheapest, fastest, largest context, most used; unknown values sort last). A non-default filter or sort is shown next to the kind tabs, and `Esc` clears the search first, then resets filter and sort, then closes. Unmodified `F2`/`F3` still work here when no global action claims them; some terminals do not report Shift+function keys.
- Each model row shows its context window, then on wide panes the median recorded speed (`t/s`) and the provider price as `$input/$output` per 1M tokens (`free` for zero-priced models). The detail lines add max output tokens, local or cloud, the roles using the model, and a numbers line: price per 1M tokens (input, output, cache read and write when the provider reports them), the speed median with its sample count and trend, average cost per priced request, request count, average latency, error rate and total recorded cost. Usage comes from the stats database (`dial stats`), read when `/models` opens and again on `F5`. Unknown values show as `-`; nothing is estimated. Requests are recorded under the backend's model id, so local checkpoints served by Dial's own engine share one label and show `-` per model.
- **Memory** is the group after Favorites in `/models` (`/model memory` opens it directly). It lists the chat models that can serve the `memory` role, leads with the one assigned, and its header shows the active memory backend and what the `memory` role is set to. Enter assigns the highlighted model to `memory`; `Ctrl+G` opens Get models on its Memory shelf. Picking the `memory` role from Roles also lands here.
- **Images** lists discovered image-generation models, including compatible checkpoints already downloaded for MLX serve. Enter assigns an image model to the `image` role rather than replacing main. Choose the role from **Roles**, or use `Ctrl+A` on a compatible model. When unassigned the image role displays **follows main**; a vision-input model is not automatically an image generator. **Get models → Images** browses Hugging Face MLX text-to-image repositories; searching reads metadata only, and Enter explicitly downloads a chosen result with the normal cancellation controls. Downloaded image models are kept out of chat and embedding role choices.
- **Decision** in Roles selects one typed-judgment engine: a chat model or `laya/decision`. Laya is selectable only when a checkpoint is configured or installed. Set the checkpoint separately with `dial laya install --checkpoint PATH` or `laya.checkpoint` in settings; it is not a model-role assignment. Planner, judge, grounding and image-embedding assignments are no longer separate roles. Vision worker models remain configurable under `vision.grounding` and `vision.image_embedding`.
  Saved `/modelpreset` bundles retain these vision selections and the Laya checkpoint separately;
  older bundles are migrated before they are applied.
- **Get models** follows Images in `/models` (`/model local` and `/models local` open it directly). Its **MLX**, **GGUF**, **Memory**, **Images**, **Embeddings** and **Curated** shelves switch with `Tab` or `Right`; `Left` returns to groups. Suggestions precede installed models. Type a query and press Enter to search Hugging Face; Enter on a result lists its files and explicitly downloads it in the background (GGUF asks which quantization first). Installed chat models assign main, or `memory` on the Memory shelf. Images downloads go to the Hugging Face cache and require a compatible image server/pack; they do not run through Dial's chat runner. Serve/import them, then F5 rescan and choose their endpoint in Images. The Embeddings shelf browses Hugging Face embedders; Curated lists discovered embedding endpoints and curated Ollama setup choices. Esc in Get models content explicitly cancels a running download first. Downloads keep running while browsing other groups or closing Models from groups, and remain inspectable through Activity → Downloads. Ending the session releases the manager and requests cancellation. Downloading never assigns a role; cancelling a pending role preparation keeps the old selection. See [model-downloads.md](model-downloads.md) and [model-embeddings.md](model-embeddings.md).
  Opening `/models` loads no model, starts no Python or MLX process and makes no network call; the installed list is read when it opens. Search, listing and downloads run only after an explicit key press, in the background. Switching to the Embedding shelf reads Ollama's model list from localhost. The same installed data is printed by `dial models status` (`--json` for the machine-readable form, with identical field names). On short panels the details give way to rows, and the list always keeps at least one row.
  - **Speed** is labelled end-to-end: output tokens per second of request wall time from the recorded stats, the same measure as
    `dial stats`. It uses the newest 40 successful requests with output and timing: a median, the median of the
    newest 20, and a trend (`up` or `down` when that median moves more than 10% from the 20 before them, else
    `flat`). It includes prompt processing time, so it is not a pure decode rate.
  - **Fit** reuses the `dial models recommend` estimate (configured or default 32768-token context, detected
    memory), with the layout from `config.json` for MLX and from the file header for GGUF.
  - **Version, adapter, gate** come from the learning store for that model under the current project's state
    directory, read with SELECT statements only. When several use cases share a model, the store changed most
    recently is shown.
  - **Waiting** lists the recorded blockers from the same rules as `/improve status`: verified turns since the
    last cycle against `[improve] lora_min_examples`, new turns against `min_new_turns`, and the remaining cooldown.
    Background-only gates (idle, AC power, `/compute` pause) are not read here, so an empty list means no recorded
    blocker, not that a cycle will start.
  - Anything unavailable is shown as `unknown` (JSON `null`): no stats database, no matching learning store, an
    unreadable store, or no memory reading. It is never estimated from a neighbouring value.

Use `/model backups` for guided fallback setup without editing TOML: filter cached/configured
chat choices, append backups, reorder them, choose failure triggers and cooldown/manual return,
then review the effective winning chain and data/cost warning before explicitly saving.
Global and Project scopes retain ordinary precedence. Browsing and saving do not contact any
provider or download models. See [guided backup models](model-routing.md#guided-backup-models).

`/model settings` (alias `/model-settings`; also `F7` in `/model`) tunes the **current main model**.
Settings are grouped into sections, with a description and the accepted range for the selected row.
PageUp/PageDown move by the visible list height; Tab jumps to the next section, including local
setting suggestions, then wraps to the first section. Shift+Tab moves back through field sections.
`↑/↓` move, `←/→` cycle a choice or toggle and step a number (the first press starts from a sensible
value), `Enter` types an exact value, `r` resets a row to its default, `s` switches between project and
global storage, `d` opens shared context defaults, and `g` opens all shared settings. Changes validate against the model's declared
capabilities and save immediately, applying on the next request; a dot marks values you changed.
Profiles match an exact provider/model pair, so switching models never reuses another model's overrides.
The **Context size** row sets a per-profile token cap (2,048–2,097,152), taking precedence over shared
context settings while still respecting model/server capacity. Reset it with `r` to follow shared
settings again. This controls history and compaction, not an external server's loaded context size.
Use `/model defaults` to open **Local context default** and **Cloud context default** directly.
This shortcut starts in global scope so the defaults persist across projects and model changes;
switch scope to set a project-only default instead. Enter a token count (for example `131072`)
or `auto` to follow model capacity. Per-model overrides and smaller loaded-server limits still apply.
Local MLX models (and LM Studio, with a subset of these controls) additionally expose top-k, min-p, XTC, seed, repetition/presence/frequency penalties with
their windows, and chat-template arguments; LM Studio rejects XTC and chat-template arguments. For an MLX model, every option its
chat template reads (for example `enable_thinking`) gets its own row: flags toggle with ←/→, options the template compares against
cycle through those values, and other options take typed text. **All template arguments** still edits the whole table as TOML.
MLX models also get **MLX server** rows: a draft model for speculative decoding (←/→ cycles installed models that share the
tokenizer; Enter types any other), draft tokens, and KV-cache bits, group size and start offset, picked from their valid values.
Ollama models get a **Keep alive** row. Those request controls are never sent to cloud providers. Startup options of externally
managed servers belong to those servers, not this page.

## Model capability icons and image previews

Model rows include monochrome icons at the right for vision, tool use, reasoning and embeddings, with a legend and capability
labels in the highlighted model's details. ASCII themes use `V`, `T`, `R` and `E`. `?` means metadata is
unknown. Typing `vision`, `tools` or `reasoning` searches supported capabilities as well as names.
Local capability declarations come from downloaded configuration and chat templates; discovery does
not start inference or load weights to produce these badges.

Viewed images and user image attachments also appear as compact inline terminal thumbnails, up to
80 columns by 16 rows, with their filename and original dimensions. Previews preserve proportions,
shrink for narrow terminals and remain in scrollback. iTerm2, Kitty and Ghostty receive a sharper
native image preview; other terminals and multiplexers use portable colored cells. Image decoding
runs on a worker while input and interruption remain responsive. The model receives the original
image bytes independently of preview size; supported cloud image requests use high detail.
