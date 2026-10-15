# Background services

The `bg` tool starts process-group-owned jobs with a bounded live output ring and a content-addressed artifact. Artifact spools are capped at 16 MiB (no configuration key changes this); when a stream exceeds the cap, the spool preserves its head and newest tail with an explicit truncation marker. The artifact digest identifies the bytes actually stored. PTY artifacts preserve raw terminal bytes; model-facing output is rendered from the terminal screen.

Jobs are session-owned: use `list`, `output`, `wait`, `input`, `resize`, and `kill` while the session is active. The `/bg` panel lists jobs and their status, and shows a PTY job's rendered screen. Enter on a PTY job attaches the keyboard to it; Ctrl-] (or Ctrl-5) detaches. Pipe input and PTY input are explicit tool actions. Processes are stopped when their owning background tool is dropped. Jobs are not persisted across a Dial process restart, and no detached-service broker is available in this build.

Output collection drains blocking readers until EOF. A descendant that escapes the owned process group and keeps an output descriptor open can therefore delay final output collection; terminate such descendants or close their descriptors before removing the job.
