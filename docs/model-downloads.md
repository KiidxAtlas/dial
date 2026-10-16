# Model downloads

Discovery never fetches weights. Searching Hugging Face, listing a repository's files and reading the
curated list only read the Hugging Face JSON API. `dial setup`, selecting a local repository for a
role, Get models downloads in `/models`, and creating the local inference backend can prepare missing
weights. Use a complete local path for offline use.

## Explicit downloads

- `dial setup -m <repo-id>` (or `--model`): validates the requested candidate, prepares and verifies
  that exact model, then saves it. A failed download or unsupported model leaves model settings unchanged.
  A successful global choice clears only this project's overrides for the selected role, just like `/models`.
- Selecting a Hugging Face repository for a role in `/models` prepares it on a background worker.
  Progress appears in the panel; closing it with Esc cancels preparation without saving the choice.
  Settings are saved only after preparation succeeds. Runtime installation also keeps the TUI responsive
  and accepts Esc to cancel; already installed files and cached weights remain available for retries.
- Get models in `/models` (see below): Enter on a search result downloads it, in the background.

A model argument is either:

- an existing **local path** (MLX directory or GGUF file). It is used as-is and never downloaded; or
- a Hugging Face **repository id** `owner/name` (letters, digits, `-`, `_`, `.`). It is used only when
  no complete local copy exists in the Hugging Face cache.

Anything else (for example `owner/name/file.gguf` or `./missing/path`) is rejected before any network
access, with a message saying it is neither a local path nor a repository id.

## Finding and downloading in `/models`

Open `/models` and choose **Get models** in the left column (`/models local` opens it directly). It has
three shelves, **MLX**, **GGUF** and **Embedding**, switched with Right/Tab (Shift+Tab goes back).
Suggested models that are not already installed appear **first**, with a selected row visible even
in short panels. Offline starting points appear immediately; a background Hugging Face catalog
request adds popular models without requiring a search. Network failures leave the starting points
usable and show an error. No browsing action downloads weights.

Each available model has a name row and metadata row: parameter count, whole-repository size,
repository publication age, downloads and likes. Details load for the selected model. Missing data
is labeled unknown, not zero. Publication age is Hugging Face's repository creation date, **not**
the original base model's release date; repository size can include multiple GGUF quantizations and
is **not** the size of a single download. The GGUF file picker shows exact per-quantization sizes.
Installed models follow suggestions, with their decode speed and memory fit.

Typing moves to the query, and Enter searches Hugging Face. Results receive list focus, so Enter
selects rather than repeating the search. Clearing the query restores offline suggestions while
refreshing the live catalog. F5 refreshes installed models and retries catalog metadata.

| Key | Action |
|---|---|
| Type, Backspace | Edit the query. Typing never searches or downloads |
| Enter (query) | Search and focus results; an empty query restores suggestions and refreshes the catalog |
| Right / Tab, Shift+Tab | Next / previous shelf (MLX is the default on Apple Silicon, GGUF elsewhere) |
| Up / Down | Move through rows; Up from the first row edits the query, Down returns to the rows |
| Enter (installed model) | Make it the main model, through the same selection as `/models` |
| Enter (result) | List the repository's files. MLX starts downloading; GGUF opens the quantization picker |
| Enter (picker) | Download the chosen quantization (with its projector, when the repository has one) |
| Enter (discovered embedding model) | Assign it to the `embedding` role |
| Enter (curated embedding model) | Set it up as `/setup embeddings:<model> yes` does; see [model-embeddings.md](model-embeddings.md) |
| Esc | Cancel a running download, close the picker or clear the query; otherwise back to the groups column (closes `/models` when that column is hidden) |
| Left (or Ctrl+Left) | Move to the groups column; Up/Down there switch groups. Downloads keep running |

A download runs in the background. The status line shows the repository, the bytes fetched so far
against the expected total, the elapsed time and the latest step. Downloads keep running while you
browse other groups in `/models`. Closing `/models` cancels a running download. When a download
finishes, Dial rescans the models (F5 does the same), and the new model is selected on its shelf. Enter
then makes it the main model.

Selecting a GGUF quantization sends exactly its files to the download (`hf_hub_download`), so the
other quantizations in the repository are neither listed for choice nor fetched. Check the file sizes
in the picker before downloading.

Chat models from the Ollama library are not pulled from this view. The Embedding shelf first lists
every embedding model discovery found (Ollama, LM Studio, other servers, local checkpoints), then the
curated Ollama embedding models not already served. Ollama models are classified by the
`capabilities` from `/api/show`. A local MLX embedding checkpoint is shown as **not served**: Dial
does not run embedding inference itself, so serve it from LM Studio or another embeddings endpoint.

## Supported formats

Dial lists the repository first, then fetches only what it needs:

| Repository contents | What is downloaded |
|---|---|
| Any `*.safetensors` (MLX) | The full snapshot, except `*.gguf` files |
| GGUF only, one model file | That model file, plus its single `mmproj*.gguf` projector if present |
| GGUF only, one complete split set (`name-00001-of-0000N.gguf` …) | All shards of that set, plus a single projector if present |

Not supported and rejected with an explanation before anything is downloaded:

- **Several GGUF quantizations** (for example `Q4_K_M` and `Q8_0`) when the repository is named by id
  alone (`dial setup -m`, `/models`). Dial will not choose one for you there. Pick one in
  Get models in `/models`, or download the file you want (`huggingface-cli download <repo> <file>`) and set the
  model to the printed local path.
- **Several multimodal projectors** (`mmproj*.gguf`), for the same reason. In the picker, quantizations
  are listed without a projector in that case.
- **Incomplete split sets**, or repositories with no safetensors and no GGUF weights (for example
  only `.bin` or `.pt` files).

## Limitations

- A download runs up to one hour. Esc cancels it; the cancelled attempt is reported as such and the
  next attempt reuses whatever Hugging Face already cached.
- A split GGUF set is downloaded shard by shard. Interrupted sets are not considered usable cached
  models; retry the same repository to fetch missing shards.
- Progress is measured from the Hugging Face cache, so the bytes shown can lag a moment behind.
  Sizes come from the repository listing; a repository that reports none shows bytes without a total.
- Search and listing use the public Hugging Face API without a token, so they see public repositories
  only. Gated or private repositories are downloaded through the Hugging Face CLI/token configured in
  the Python environment, as with `dial setup -m`.
- Downloaded files are kept in the standard Hugging Face cache and are reused on later runs; complete
  local copies are never fetched again.
