# Voice

Voice is off by default. In `/settings`, open **Voice**, then enable **Dictation** in the **Dictation** section and choose a **Recognition program**. The corresponding config keys are under `[voice.tts]` and `[voice.stt]`.

Selecting a missing local speech or recognition program in the Voice settings asks whether to
install and configure it. Press Enter again to confirm, or Esc to cancel. Setup runs in the
background and selects the backend only after it succeeds; failures and cancellation preserve
the previous selection. Required local voice/model files are prepared too. Already configured
backends are selected immediately. OpenAI needs an API key through `/providers`, rather than a
package install; macOS `say` is supplied by the operating system. Read-aloud and dictation remain
separate toggles, so installing a backend does not turn on the microphone or automatic speech.

## Read answers aloud

- In `/settings` → **Voice** → **Spoken replies**, turn on `voice.tts.auto_speak` to hear each final agent answer. It is off by default and independent of microphone input. Answers longer than `voice.tts.auto_speak_max_chars` are cut short.
- Press the speech key, Esc, or send a new prompt to stop a read-out in progress. Starting microphone capture stops playback so it is not transcribed as your speech.
- `voice.tts.backend` picks the program: `auto` (first installed), `say` (built into macOS), `piper`, `espeak` (espeak-ng), or `openai`.
- The `tts` tool uses the same backend.
- A missing program is reported as an error; nothing is claimed as spoken when it was not.

## Speak a turn

- Enable **Dictation** in `/settings` → **Voice** and choose a **Recognition program**. The microphone only listens after you press the speech key; it is not an always-on listener.
- The default speech keys are `Option+V` on macOS (`Alt+V` elsewhere) and `F9`. `/hotkeys` shows the active bindings and lets you remap them.
- Press once, speak, then pause: recording ends after trailing silence and the transcript is sent as a normal user turn. You can also press again to finish sooner. A short key tap no longer selects hold-to-talk just because the terminal supports key releases.
- On macOS, terminals that send `√` instead of an Alt key event are supported too: Dial treats that key as Option+V while `Alt+V` is bound to speech input. This reserves typed `√` for the shortcut (even when speech recognition is off); bracketed paste still inserts it literally. Remapping away from `Alt+V` disables this fallback. Other layouts can use `F9` or remap in `/hotkeys`.
- `voice.stt.backend` picks the recognition program: `auto`, `openai`, `whisper-cpp` (whisper-cli), or `mlx-whisper`.

### Speech key behavior

In Settings, **Speech key behavior** (`voice.stt.input_mode`) offers:

- **Press to talk** (`toggle`, default): press once to listen; pause or press again to finish.
- **Hold to talk** (`hold`): hold the key while speaking and release it to finish. This requires terminal key-release reporting; otherwise Dial falls back to press-to-talk. Silence does not end a held recording.

The composer shows the active shortcut and how to stop. Both modes stop at `voice.stt.max_recording_secs`.

### Sending or reviewing speech

**Send speech** (`voice.stt.submit_trigger`) controls what happens after recognition:

- **When recording finishes** (`release`, default): send the transcript, including single-word replies, without Enter.
- **Never (review first)** (`never`): insert the transcript into the composer; review and press Enter yourself. Set this to restore dictation-only behavior.
- **After a complete sentence** (`release-complete`): send only if the transcript ends in `.`, `?`, or `!`.
- **When I say submit** (`say-submit`): send only if the final word is “submit”; that word is removed.

Existing drafts are never sent automatically: speech is inserted for review instead. Transcripts starting with `/` or `!` also stay in the composer so recognition cannot automatically run commands. Empty or failed recordings do not send turns. While an agent is working, automatically submitted speech goes into the existing after-turn queue rather than interrupting its work. A full queue keeps the transcript in the draft.

### Recording problems

Stopping a recording preserves audio already queued by the microphone. Recognition starts only
after capture stops, so a slow backend cannot delay release/deadline handling. Queued audio and
buffered speech are bounded by the maximum recording duration.
Failures distinguish no microphone audio, a recording shorter than the minimum speech duration,
audio without detected speech, and a recognition backend returning no text. For missing or quiet
audio, check the default input device and microphone level; on macOS, also check the terminal's
microphone permission in System Settings → Privacy & Security → Microphone. If you select hold
mode, keep the key held while speaking. In the default press-to-talk mode, release it and speak.

## Live voice

Protocol clients (not the TUI) run hands-free sessions over the RPC control channel:

- `live_start` (optional `voice` and `instructions`) starts a session, replacing any previous one; `[voice.live]` supplies the defaults.
- `live_mute` (`muted`) silences spoken replies. Muting also stops a reply that is being read.
- `live_stop` ends the session and stops any reply being read.
- `live_listen` records one utterance (ends on silence; needs `voice.stt.enabled`), returns the `transcript`, and runs it as a turn. `abort` cancels the recording.

While a session is active, every `send` text is prefixed with the session `instructions`, and each finished answer is read aloud. A new `send` or `live_listen` stops the reply being read. Speech failures arrive as `live_error` events. Live sessions are not available over ACP.
