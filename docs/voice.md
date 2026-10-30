# Voice

Voice is off by default. Settings are under the Voice tab in `/settings`, or in `[voice.tts]` and `[voice.stt]` of the config.

Selecting a missing local speech or recognition program in the Voice settings asks whether to
install and configure it. Press Enter again to confirm, or Esc to cancel. Setup runs in the
background and selects the backend only after it succeeds; failures and cancellation preserve
the previous selection. Required local voice/model files are prepared too. Already configured
backends are selected immediately. OpenAI needs an API key through `/providers`, rather than a
package install; macOS `say` is supplied by the operating system. Read-aloud and dictation remain
separate toggles, so installing a backend does not turn on the microphone or automatic speech.

## Read answers aloud

- `voice.tts.auto_speak = true` reads each final answer aloud when a turn finishes. Answers longer than `voice.tts.auto_speak_max_chars` are cut short.
- Press Esc, or send a new prompt, to stop a read-out in progress.
- `voice.tts.backend` picks the program: `auto` (first installed), `say` (built into macOS), `piper`, `espeak` (espeak-ng), or `openai`.
- The `tts` tool uses the same backend.
- A missing program is reported as an error; nothing is claimed as spoken when it was not.

## Dictation (push-to-talk)

- Turn on `voice.stt.enabled`. The default push-to-talk key is `Option+V` on macOS and `Alt+V` elsewhere. `/hotkeys` shows the active binding, including remaps.
- On macOS, terminals that send `√` instead of an Alt key event are supported too: Dial treats that key as Option+V while `Alt+V` is bound to dictation. This reserves typed `√` for the shortcut (even when speech recognition is off); bracketed paste still inserts it literally. Remapping dictation away from `Alt+V` disables this fallback. This fallback covers layouts where Option+V produces `√`; other layouts can use `F9` or remap in `/hotkeys`.
- The transcript is inserted into the draft. It is never sent on its own; review it and press Enter.
- `voice.stt.backend` picks the program: `auto`, `openai`, `whisper-cpp` (whisper-cli), or `mlx-whisper`.

### Hold to talk

Used when the terminal reports key releases (the kitty keyboard protocol with event types). Dial requests it at startup and uses hold mode only when the terminal accepts it.

- Hold the dictation shortcut to record. Release it to stop; the transcript goes into the draft.
- Recording does not end on silence while the key is held. It ends at release, or at `voice.stt.max_recording_secs`.
- The composer border shows the active shortcut and asks you to release it to stop.

### Press to toggle

Used when the terminal does not report key releases, or does not support the keyboard protocol.

- Press the dictation shortcut to start recording. Press it again to stop, or let recording stop by itself after a silence or after `voice.stt.max_recording_secs`.
- The composer border shows the active shortcut to stop recording.

Both modes insert the transcript into the draft and show "No speech was heard." when nothing was recognised.

### During a turn

- The dictation shortcut works while a turn is running. The transcript is inserted into the draft while the turn keeps going; nothing is interrupted.
- Submit the draft with Enter as you would any other message typed during a turn.

## Live voice

Protocol clients (not the TUI) run hands-free sessions over the RPC control channel:

- `live_start` (optional `voice` and `instructions`) starts a session, replacing any previous one; `[voice.live]` supplies the defaults.
- `live_mute` (`muted`) silences spoken replies. Muting also stops a reply that is being read.
- `live_stop` ends the session and stops any reply being read.
- `live_listen` records one utterance (ends on silence; needs `voice.stt.enabled`), returns the `transcript`, and runs it as a turn. `abort` cancels the recording.

While a session is active, every `send` text is prefixed with the session `instructions`, and each finished answer is read aloud. A new `send` or `live_listen` stops the reply being read. Speech failures arrive as `live_error` events. Live sessions are not available over ACP.
