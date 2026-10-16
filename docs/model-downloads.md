# Model downloads

Discovery never fetches weights. `dial setup`, selecting a local repository for a role, and creating
the local inference backend can prepare missing weights. Use a complete local path for offline use.

## Explicit downloads

- `dial setup -m <repo-id>` (or `--model`): prepares the model, then verifies it.
- Selecting a Hugging Face repository for a role in `/models` also prepares it at that moment.

A model argument is either:

- an existing **local path** (MLX directory or GGUF file). It is used as-is and never downloaded; or
- a Hugging Face **repository id** `owner/name` (letters, digits, `-`, `_`, `.`). It is used only when
  no complete local copy exists in the Hugging Face cache.

Anything else (for example `owner/name/file.gguf` or `./missing/path`) is rejected before any network
access, with a message saying it is neither a local path nor a repository id.

## Supported formats

Dial lists the repository first, then fetches only what it needs:

| Repository contents | What is downloaded |
|---|---|
| Any `*.safetensors` (MLX) | The full snapshot, except `*.gguf` files |
| GGUF only, one model file | That model file, plus its single `mmproj*.gguf` projector if present |
| GGUF only, one complete split set (`name-00001-of-0000N.gguf` …) | All shards of that set, plus a single projector if present |

Not supported and rejected with an explanation before anything is downloaded:

- **Several GGUF quantizations** (for example `Q4_K_M` and `Q8_0`). Dial will not choose one for you.
  Download the file you want (`huggingface-cli download <repo> <file>`) and set the model to the
  printed local path.
- **Several multimodal projectors** (`mmproj*.gguf`), for the same reason.
- **Incomplete split sets**, or repositories with no safetensors and no GGUF weights (for example
  only `.bin` or `.pt` files).

## Limitations

- Downloads are not cancellable from inside Dial; they run up to one hour.
- A split GGUF set is downloaded shard by shard. Interrupted sets are not considered usable cached
  models; retry the same repository to fetch missing shards. A directory containing several complete
  quantizations requires selecting an explicit file rather than silently choosing one.
- Authentication for gated or private repositories relies on the Hugging Face CLI/token configured in
  the Python environment.
- Downloaded files are kept in the standard Hugging Face cache and are reused on later runs; complete
  local copies are never fetched again.
