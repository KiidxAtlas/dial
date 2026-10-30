# Composer and editor

The prompt editor is a grapheme-aware multiline editor whose editing operations are keymap `Action`s, so bindings remain inspectable and rebindable. Consecutive typing is coalesced into undo units; kill operations feed a bounded kill ring with yank and yank-pop. Word movement and deletion are implemented by the editor. Long bracketed pastes are represented by a draft placeholder and expanded on submit, so the visible prompt does not need to render an unbounded payload.

Collapsed-paste payloads survive draft replacement, clear, undo and redo. They are retained only while their marker is recoverable from the current or saved history-browsing draft, the editor's bounded undo/redo history, or its bounded kill ring; unreachable payloads are discarded during refresh.

Successful submission or queueing ends that draft's undo/redo and paste-payload lifetime. History retains the expanded prompt; a placeholder typed into a later draft cannot resurrect a previously sent payload.

`Ctrl+R` searches recent in-memory prompts immediately and queries the configured history provider with the entered case-insensitive substring (up to 500 matching results, not just the newest 500 prompts). Provider lookups are debounced and serialized off the input thread, with only the newest pending query retained. Responses from dismissed searches, older queries or replaced providers are ignored. While a lookup runs, existing matches remain usable.

Autocomplete candidates are ranked on worker threads and delivered back to the composer tagged with the originating query. Results are only applied while that query still matches the active menu. Refresh project candidates outside keystroke handling with `Composer::set_files` and `Composer::set_commands`.

Image file paths explicitly pasted into a prompt are recognized during user-message expansion and converted into integrity-checked local image references. Clipboard image paste uses platform clipboard utilities and stores temporary image files for the lifetime needed by submission. The LLM image layer verifies references and applies model-specific resizing before inference. Tool-originated image references remain constrained by the project permission rules.

Word completion offers the rest of the word under the cursor as dim ghost text when no history or command-argument suggestion applies. Candidates come from two sources. The first is a learned model in `learned-words.json` under the Dial home directory. Prompts are learned when submitted, including queued follow-ups, and older words decay as newer prompts arrive. The second is a project index of identifiers and file stems, built on a worker thread from the workspace file list and cancelled when the list is refreshed. Prefixes shorter than two characters get no suggestion. Text that looks like a credential is never learned. The file is stored with owner-only permissions, and it is capped at 20,000 words. `Tab` or `Right` accepts the suggestion. The setting is `ui.word_completion` (default `true`), which is also the "Word completion" row in Settings. Turning it off stops both suggestions and learning.

## Commands during a turn

Live slash commands run immediately rather than queueing a follow-up or stopping the turn.
`/retry` takes no arguments and requests an immediate provider retry only while a retry wait is
pending. It skips the remaining automatic or server-requested delay without cancelling the turn
or increasing the attempt limit. During active inference or other work with no pending provider
retry wait, it reports that no wait is pending and does not save a request for later. When idle,
`/retry` instead retries the last failed, unanswered turn, as before. See
[LLM transport behavior](llm-transport.md) for automatic retry timing and bounds.

Editor behavior is covered in `crates/dial-ui/src/ui/editor.rs` and composer behavior in `crates/dial-ui/src/ui/composer.rs`; run `cargo test --workspace --lib ui::editor` and `cargo test --workspace --lib ui::composer` from the repository root.
