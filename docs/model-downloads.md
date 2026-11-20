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
six shelves, **MLX**, **GGUF**, **Memory**, **Images**, **Embeddings** and **Curated**, switched with
Right/Tab (Shift+Tab goes back). The Memory shelf suggests small 1-2B instruct models (in this
machine's default weight format) for the `memory` role and lists installed chat models whatever
their format; Enter on an installed model assigns it to `memory`. In the Memory group of
`/models`, `Ctrl+G` opens this shelf.
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

The **Images** shelf searches Hugging Face for **Diffusers** repositories tagged **text-to-image**,
the layouts Dial runs itself, not models guessed from names or vision-input capabilities. Browsing
and searching only read metadata. Enter on a result lists its files and downloads only what
stable-diffusion.cpp loads to the **Hugging Face cache**: `model_index.json`, each component's
config and its half-precision (`fp16`) weights when offered, else its default weights. Duplicate
single-file checkpoints, sample images and other precision variants are skipped. FLUX.2 repositories
(such as FLUX.2 klein) are the exception: stable-diffusion.cpp cannot read their Diffusers
`Flux2Transformer2DModel` transformer (it misdetects it as FLUX.1 and refuses its tensors), so Dial
downloads the original single-file transformer at the repository root instead and loads it with the
Diffusers VAE and Qwen3 text encoder. A FLUX.2 model downloaded before this change lacks that file;
its first `image_gen` call is refused with a note to download it again from the Images shelf. Esc
cancels a download.
Dial runs a downloaded checkpoint itself on stable-diffusion.cpp's `sd-server`, on macOS, Linux
and Windows: a single `.safetensors`/`.ckpt` file, a Diffusers UNet layout (SD 1.x/2.x/XL) or a
Diffusers transformer layout (FLUX, SD3, Krea 2 and similar). Enter on it assigns the `image` role,
and each `image_gen` call starts `sd-server` on loopback, loads the checkpoint and stops the server
afterwards, so every call pays the model load. When no `sd-server` is installed, the first
`image_gen` call asks to install one; yes installs the same pinned, checksum-verified release as
`/setup sd-cpp` into Dial's `tools/sd-cpp` folder (macOS arm64, Linux x64, Windows x64).
`DIAL_SD_SERVER` points Dial at another build, such as a Vulkan or CUDA release. Whether
stable-diffusion.cpp supports a given architecture is known only once it loads; a load failure
quotes the server's log.

MLX-quantized packs (mflux 4/8-bit, MLX-Serve conversions) store packed integer weights that
stable-diffusion.cpp cannot read. Enter on one, in the **Images** group or shelf, looks up the
model the pack declares as its `base_model` on Hugging Face and asks whether to download that
original (with its size); `y` downloads it as above, `n` or Esc downloads nothing. Gated originals
are refused with the license and `hf auth login` steps. On macOS with Apple Silicon the pack itself
is available instead: assigned to the `image` role, it runs on an MLX-Serve that `image_gen` starts
for each call and installs on first use (see [image generation](image-generation.md)). An
MLX-Serve you run yourself still works: F5 rescan and select its endpoint. Roles / `Ctrl+A` offer the same
assignment. Image-only checkpoints are not assigned to `main`, `memory` or `embedding`. An
unassigned `image` role follows the main model; this does not turn a chat-only model into an
image-generation endpoint.

| Key | Action |
|---|---|
| Type, Backspace | Edit the query. Typing never searches or downloads |
| Enter (query) | Search and focus results; an empty query restores suggestions and refreshes the catalog |
| Right / Tab, Shift+Tab | Next / previous shelf (MLX is the default on Apple Silicon, GGUF elsewhere) |
| Up / Down | Move through rows; Up from the first row edits the query, Down returns to the rows |
| Enter (installed model) | Assign chat to main or memory; image weights require a compatible served endpoint |
| Enter (result) | List the repository's files. MLX starts downloading; GGUF opens the quantization picker |
| Enter (picker) | Download the chosen quantization (with its projector, when the repository has one) |
| Enter (discovered embedding model) | Assign it to the `embedding` role |
| Enter (curated embedding model) | Set it up as `/setup embeddings:<model> yes` does; see [model-embeddings.md](model-embeddings.md) |
| Esc (Get models content) | Explicitly cancel a running download; otherwise close the picker, stop listing, or clear the query before returning to groups |
| Left (or Ctrl+Left) | Move to the groups column; Up/Down there switch groups. Downloads keep running; Esc from groups closes Models without cancelling them |
| Ctrl+D (installed model) | Ask to delete it from disk; Enter or `y` deletes, Esc or `n` keeps it. Also works on local rows of the other model lists |

### Deleting local models

Ctrl+D deletes a downloaded local model wherever `/models` lists it: in All, Favorites, Memory,
Images and provider groups (any row backed by a file or folder on this machine, marked
`Ctrl+D delete` in the footer) and on installed models in Get models. One confirmation shows the
model, where it came from, how much space it frees and the folder that will be removed; Enter or
`y` deletes, Esc or `n` keeps it. Afterwards the lists rescan, so the model disappears everywhere.

Dial deletes only models its local discovery found, and only their own folder inside the store
that holds them: the whole `models--owner--name` repository of the Hugging Face cache (snapshots
share its blobs, so the repository is the unit), or the `publisher/model` folder of LM Studio's
download folder or `~/.mlx-serve/models`. Models outside those stores, linked folders and the stores
themselves are refused with the reason. A model the configuration selects, as the main model or
for any role, is refused before the confirmation until another model is chosen, and deleting waits
for a running download. Models served by Ollama or another server are managed by that server.
Clients use the RPC method `delete_local_model` (`model`, optional `dry_run`).

A download runs in the background. The status line shows the repository, the bytes fetched so far
against the expected total, the elapsed time and the latest step. Get models and Activity → Downloads
share the session's download manager: browsing another group or closing `/models` from the groups
column does **not** cancel an explicitly started weight download. Reopen Models or Activity to
inspect it. Esc while focused on Get models content explicitly requests cancellation instead of
closing the panel; ending the session releases its final owner and requests cancellation too.
Cancellation can leave already cached files; it is not a rollback of downloaded bytes.

When a download finishes, Dial rescans the models (F5 does the same), and the new model is selected
on its shelf. Enter then assigns chat to main or memory. Image downloads require serving/importing
before selecting the endpoint in Images. Downloading alone never assigns a model.

This differs from **preparing a role assignment**: cancelling that pending preparation keeps the
previous selection and does not save the new role. Browsing, downloading weights, preparing an
assignment, saving it, and explicitly cancelling work are separate actions.

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

- A download runs up to one hour. Esc in Get models content explicitly cancels it; closing the
  Models browser from groups does not. The cancelled attempt is reported as such, and the next
  attempt reuses whatever Hugging Face already cached.
- A split GGUF set is downloaded shard by shard. Interrupted sets are not considered usable cached
  models; retry the same repository to fetch missing shards.
- Progress is measured from the Hugging Face cache, so the bytes shown can lag a moment behind.
  Sizes come from the repository listing; a repository that reports none shows bytes without a total.
- Search and listing use the public Hugging Face API without a token, so they see public repositories
  only. Gated or private repositories are downloaded through the Hugging Face CLI/token configured in
  the Python environment, as with `dial setup -m`.
- Downloaded files are kept in the standard Hugging Face cache and are reused on later runs; complete
  local copies are never fetched again.
