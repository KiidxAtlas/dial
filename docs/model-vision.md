# Vision model roles: grounding and image embeddings

Two optional model roles give tools their own vision models. Each role does nothing until it is assigned.

| Role | Used by | Unassigned |
| --- | --- | --- |
| `grounding` | `computer` `target.locate(description)` and `browser` `action=locate`: find a described UI element in a screenshot | Both calls fail with a message that names `/setup grounding` |
| `image_embedding` | The read-only `image_search` tool, and a prompt-similarity line after `image_gen` writes an image | `image_search` is not offered and no similarity line is added |

Both roles run on Dial's local MLX model server on Apple Silicon (the pinned `mlx-vlm` runtime) or
on any OpenAI-compatible endpoint (`chat-completions` protocol). Other protocols, such as Anthropic,
Responses or native Gemini, are refused with that message.

## Easy install

`/setup grounding` and `/setup image-embeddings` list the curated models. The guided `/setup` menu
has a row for each. `/setup <id>:<repo>` previews the download size, cache state and the role it
will set. `/setup grounding yes` (or `/setup grounding:<repo> yes`) runs the setup in the
background:

1. It installs Dial's MLX runtime first if `python/.venv` is missing (macOS arm64; uv is needed).
2. It downloads the Hugging Face checkpoint into the shared model cache. A cached checkpoint is
   reused.
3. It assigns the role globally (`model_roles.grounding` or `model_roles.image_embedding`, with
   `provider = "mlx"`) and reloads the running session, so the tools appear without a restart.

A project-scoped role assignment still overrides the new global one, and the summary says so.
Any model can also be assigned to either role in Alt+M (/roles), or in configuration:

```toml
[model_roles.grounding]
provider = "mlx"
model = "mlx-community/UI-Venus-1.5-2B-6bit"

[model_roles.image_embedding]
provider = "mlx"
model = "mlx-community/Qwen3-VL-Embedding-2B-8bit"
```

Curated models (sizes rounded):

| Role | Model | Download | Notes |
| --- | --- | --- | --- |
| grounding | `mlx-community/UI-Venus-1.5-2B-6bit` | 2.5 GB | Default. Qwen3-VL based; answers in 0–1000 coordinates |
| grounding | `mlx-community/UI-TARS-1.5-7B-4bit` | 4.9 GB | Larger and slower. Qwen2.5-VL based; answers in resized-image pixels |
| image_embedding | `mlx-community/Qwen3-VL-Embedding-2B-8bit` | 2.7 GB | Default. 2048-dimension text and image vectors in one space |

## Grounding

The grounding model receives the screenshot (scaled to at most 1920 px on the long edge) and the
element description. Its reply is parsed according to the model family's coordinate convention:

- 0–1000 for Qwen3-VL and UI-Venus;
- smart-resized pixels for UI-TARS and Qwen2.5-VL;
- 0–1 for ShowUI;
- auto-detected otherwise.

The parsed point is then mapped back to screenshot pixels. The model is loaded on first use and
stays resident.

- **Computer:** `await target.locate("the blue Save button")` (JavaScript) or
  `target.locate(...)` (Python) captures the target, which becomes the input frame. It returns
  `{x, y, width, height, target, model}` in that screenshot's pixels. It never clicks: pass the
  point to `click` and check the result. Available on desktop, display and window targets.
- **Browser:** `action=locate` with `description` (at most 500 characters) captures the viewport.
  It returns `x`/`y` in viewport CSS pixels, ready for `click_at`. `full_page` is refused.

Grounding is a vision model's estimate. Verify each action with a screenshot.

## Image search

`image_search` is read-only and needs no approval. It has three modes:

- `mode=text`: ranks images under `path` (default: the workspace root) against `query`.
- `mode=similar`: ranks images under `path` by likeness to the reference `image`. The reference
  itself is excluded.
- `mode=score`: scores each of `images` (at most 32) against the prompt `query`.

Scores are cosine similarities. Compare them within one call, not against fixed thresholds. The
scan:

- honours ignore files;
- skips hidden, `.git`, `node_modules`, `target`, `.venv` and protected Dial paths;
- reads PNG, JPEG, WebP and GIF files up to 20 MiB;
- considers at most 2,000 images per scope.

Vectors are cached by image content (SHA-256) and model identity in
`~/.dial/projects/<project>/image_embeddings.sqlite3`, outside the workspace. Each call embeds at
most 256 new images; repeat the call to include the rest.

After `image_gen` writes an image while the role is assigned, its result gains a line such as
`Prompt similarity 0.312 (…)`. If embedding fails, the line reports the error and the generated
image is kept.

## Limits

- The local route needs macOS on Apple Silicon. Elsewhere, serve the model from an
  OpenAI-compatible endpoint: vLLM or LM Studio for grounding, and a `/embeddings` endpoint that
  accepts `image_url` items for image embeddings.
- The embedding checkpoint declares the same architecture as a chat VLM. Dial starts it with an
  explicit embedding flag. Assigned to a chat role, it would not chat.
- Quality has been checked only on small local fixtures: a synthetic dialog, where Venus found
  Save within 6 px, and three images, where "a red circle" ranked the red circle first. This is
  not a benchmark.
