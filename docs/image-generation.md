# Image providers

`/models` → **Images** selects the `image` role; it does not change the main chat model.
Image input/vision is not evidence that a model can generate images. Known image models and explicit
`model_profiles.<name>.capabilities.image_generation` declarations identify the generation transport.
ChatGPT plan models are also listed: their catalog omits the hosted image tool, so assigning one is
an approved attempt through the Responses hosted tool with your subscription, even while main uses
another provider; an explicit `disabled` declaration still refuses it.
Provider credentials and model access are still required. A catalog entry is not a live generation test.
Direct Gemini API image models (`gemini-2.5-flash-image`, `gemini-3-pro-image`, `gemini-3.1-flash-image`,
`gemini-3.1-flash-lite-image`, `nano-banana-pro-preview`, `gemini-nano-banana-2.1` and their
`-preview` ids) are listed with the `gemini` transport. A saved catalog from an older version picks
them up when it is loaded; no manual rescan is needed.

## Use it with any chat model

With `image.auto = true` (the default), ask your chat model to generate an image or run
`/image generate <prompt>`. Opus and other chat models can use the tool without selecting an
image role. Existing image settings, configured image profiles and saved OpenAI/xAI credentials
still determine which service generates the image; the chat model is not changed.

If no automatic image route is configured, the model-callable tool remains available and reports
the setup requirement when called rather than silently disappearing as "not enabled". Offering
the tool does not provide credentials or make the chat model itself an image generator. Configure
an image provider through `/providers`, or choose an image service under `/models` → **Images**.
Explicit image-route errors remain visible, and `image.auto = false` still disables automatic
offering (an explicit `integrations = ["image_gen"]` entry enables it separately). All generation
continues to require approval.

To build a page or screen on generated art (generate, inspect, build the UI, confirm with a browser
screenshot, remix), import the versioned `art-to-ui` example skill with
`dial skills import ./examples/skills/art-to-ui`; see [skills](skills.md#import-and-maintain-the-examples).

## Transports

| Declaration (`image.api` or profile capability) | Service | Source-image edits |
| --- | --- | --- |
| `images` | OpenAI Images; xAI uses its native aspect-ratio fields on the same route | OpenAI gpt-image and DALL·E 2 only |
| `mlx-serve` | MLX-Serve Images API | Yes; PNG output |
| `responses` | Responses hosted image tool, including the approved ChatGPT attempt | Yes |
| `chat-completions` | Inline image output, for example OpenRouter | No |
| `gemini` | Gemini native `generateContent` | Yes |
| `imagen` | Explicit Vertex/compatible Imagen `:predict` endpoint | No |
| `black-forest-labs` | BFL FLUX generation and returned polling URL | No |
| `comfyui` | Explicit ComfyUI API-format workflow | No |
| `fal` | fal model endpoint and queue | No |
| `replicate` | Replicate prediction | No |

Edit support in this table is what **Dial's adapter** accepts, not a statement that other APIs or
workflows cannot edit images. Unsupported edits fail before submission. Nothing is uploaded merely
by choosing an image role or opening image settings.

## Dedicated endpoint

Use the image role for normal picker-based selection. An explicit `[image]` endpoint is useful for
native services, self-hosted endpoints, and project-specific Vertex paths. It overrides the role:

```toml
[image]
api = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta"
model = "gemini-2.5-flash-image"
api_key_env = "GEMINI_API_KEY"
timeout_secs = 180
```

`api` is optional for existing configurations: omitting it preserves OpenAI/xAI host inference.
Local HTTP endpoints may omit credentials. Remote services require HTTPS and a credential, from the
named environment variable or the provider's saved key. Do not put secrets in the URL or workflow.

Other dedicated endpoint combinations:

| API | Base URL | Example model | Credential variable |
| --- | --- | --- | --- |
| `images` | `https://api.openai.com/v1` | `gpt-image-1.5` | `OPENAI_API_KEY` |
| `images` | `https://api.x.ai/v1` | `grok-imagine-image-2.0` | `XAI_API_KEY` |
| `mlx-serve` | `http://127.0.0.1:11234/v1` | Exact model ID reported by your server | None locally |
| `black-forest-labs` | `https://api.bfl.ai/v1` | `flux-2-pro` | `BFL_API_KEY` |
| `fal` | `https://queue.fal.run` | `fal-ai/flux/schnell` | `FAL_KEY` |
| `replicate` | `https://api.replicate.com/v1` | `black-forest-labs/flux-schnell` | `REPLICATE_API_TOKEN` |
| `comfyui` | `http://127.0.0.1:8188` | A chosen workflow label | None locally |

For a profile instead, set `capabilities.image_generation` to the same transport name and configure
its provider endpoint. An explicit `disabled` capability prevents generation through that model.

The automatic OpenAI default is `gpt-image-1.5`. OpenAI shut down `gpt-image-1` on October 23,
2026; explicitly configured old IDs are not silently rewritten to another model.
See [OpenAI's image generation guide](https://developers.openai.com/api/docs/guides/tools-image-generation).

### Imagen and Vertex

Google's [Gemini Imagen guide](https://ai.google.dev/gemini-api/docs/imagen) says Imagen has shut down
on the Gemini API. Do **not** use a Gemini API key and expect Imagen to work there. Use a Gemini image
model instead, or configure your own project/region-specific Vertex endpoint:

```toml
[image]
api = "imagen"
base_url = "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google"
model = "imagen-4.0-generate-001"
api_key_env = "VERTEX_ACCESS_TOKEN"
```

`VERTEX_ACCESS_TOKEN` must contain a currently valid Google Cloud access token for that project.
Dial reads it for each request; this adapter does not obtain or refresh Cloud OAuth credentials.
Project billing, IAM, regional availability and model permissions are operator responsibilities.
The Imagen adapter rejects the Gemini API host before submitting. Landscape and portrait sizes map
to Imagen's supported 4:3 and 3:4 ratios rather than Gemini's 3:2 and 2:3; `auto` leaves the API default.
Imagen output is PNG or JPEG; a WebP destination is rejected before submission.

## Per-model settings

`/image settings` shows only fields supported by the chosen adapter. Settings match an exact model
ID; they are shared between uses of that model. A call's explicit size wins over a configured size.
Native providers may implement a size as an aspect ratio rather than exact pixel dimensions.

For fal and Replicate, **Model input** edits the model-specific `input` table. Consult the selected
model's schema, since different models accept different fields. For example:

```toml
[image.models.replicate_flux]
model = "black-forest-labs/flux-schnell"
input = { go_fast = true }
```

The adapter supplies the prompt and enforces single-image output. Input settings cannot override
Dial's prompt, requested output format, or output count. BFL FLUX.2 routes accept the shared pixel
size control; other BFL models, fal and Replicate use `auto` when size is omitted and refuse explicit
pixel sizes. Set model-specific dimensions or aspect ratios in fal/Replicate `input` instead.
Invalid or unsupported provider input is reported, never retried against another model.

The generic queue routes require a text-to-image model accepting `prompt` and `output_format`,
plus `num_images` (fal) or `num_outputs` (Replicate). The example FLUX Schnell models document those
fields. Models with a different input/output schema are not interchangeable; consult their schema
before submitting. fal output is PNG or JPEG; this adapter rejects WebP before submission.

### ComfyUI workflows

A ComfyUI server is not one universal image model. Export the installed workflow in **API format**,
then store its full node graph in `image.models.<key>.comfyui.workflow`. Set `prompt_node` to the node
whose `inputs.text` receives the prompt, and `output_node` to the image-producing output node. The
entry's `model` must exactly match `[image].model` or the selected profile's model ID.

The **Workflow** row in `/image settings` edits a TOML inline table with these three keys:

- `workflow`: the full exported node-ID → node-object graph (not the UI-format `nodes`/`links` file).
- `prompt_node`: the string node ID containing the positive prompt's string `inputs.text`.
- `output_node`: the string node ID whose completed output contains exactly one image.

Keep checkpoint names, negative prompts, dimensions, sampling, seeds, LoRAs and batch size in the
workflow itself. Use batch size one. Dial changes only the configured prompt node; it does not guess
node IDs, rewrite the graph, install missing models/nodes, or upload source images. Omit the tool's
`size` argument or set it to `auto`; an explicit pixel size is refused. ComfyUI may execute arbitrary
installed custom nodes or paid API nodes in a configured workflow, so review the graph before using it.

## Actual layer-weight edits (local checkpoints)

Under **Settings → Image generation**, enable **Allow layer weight edits**
(`image.allow_weight_edits = true`, off by default). This enables the approval-gated `image_weights`
tool to inspect and scale actual tensor values, not LoRA strengths. Enabling the setting alone never
changes weights. No Python/MLX runtime, model download or inference request is needed for the edit.

1. Grant access to the local model directory through the normal directory-access workflow. Discovery
   of an MLX-Serve model does not authorize access to its files.
2. Ask Dial to inspect a specific `.safetensors` file or shard. Inspection returns sorted, paged tensor
   names, shapes, dtypes and a source SHA-256. `prefix`, `offset` and `limit` narrow the listing.
3. Ask it to scale an explicit map of **exact tensor names** to multipliers, using that source hash and
   a **new** output `.safetensors` path. For example, a selected attention weight can be multiplied
   by `0.9`. Factors must be finite and within `[-8, 8]`; at most 128 tensors can be selected per call.
4. To use the result, put the exported file into a **separate compatible copy** of the complete model
   (retaining its component directories, shard names/index and configuration), then explicitly load
   that copy in MLX-Serve. The tool does not copy the entire model, change routing or reload a server.

Supported selected tensor dtypes are F16, BF16, F32 and F64. Packed quantized/integer tensors are
refused rather than incorrectly multiplying their encoded bits. Quantized base weights need an
explicit dequantization/requantization workflow outside this tool. Unknown tensor names, stale
source hashes, nonfinite values and overflow fail without publishing an output. Tensor names, shapes,
dtypes, metadata and all unselected bytes are preserved. Existing files and source checkpoints are
never overwritten. Each file must fit in RAM and be at most 8 GiB; use individual smaller shards for
larger models. Cancellation before publication removes the staged file.

Scaling tensors can worsen or break image generation. It is not training, does not guarantee better
images, and does not change a model already loaded by MLX-Serve. Keep the original model available.

## Submission, downloads and cancellation

All generation remains approval-gated. `/image generate <prompt>` and the model-callable `image_gen`
use the same route, destination preflight, decoding limits and no-overwrite publication. The source
image is read-only. A completed image must be PNG, JPEG or WebP. Dial validates it under dimension
and allocation limits, then converts it locally if necessary to match the output extension. Matching
formats retain the original bytes; JPEG conversion is lossy and drops transparency. This also lets
the PNG-returning MLX-Serve route save JPEG or WebP, and lets `/image` save JPEG/WebP provider
results as PNG, without another generation request. A paid call can still fail if the provider returns
invalid image data, conversion fails, or the final filesystem write fails.

Queue adapters submit **once**, then poll that job. They do not retry submissions, switch providers,
or silently switch billing routes. `image.timeout_secs` bounds the entire native operation, including
polling and downloading (`0` disables the deadline). Interruption stops waiting and prevents output
publication, but it does **not** guarantee that a remote job was canceled or that charges stopped.
Failures after a queued submission include its job ID when one is known.

Credentialed polling stays on the service's origin, with BFL's documented regional routing exception.
Image downloads use a separate request with **no** API key or custom request headers, no redirects,
a byte limit, and documented provider delivery hosts (or the configured service's own origin).
Arbitrary URLs returned by a service are refused. Providers using an additional delivery domain need
an explicit adapter update rather than a permissive redirect or credential-forwarding workaround.
When a route carries custom request headers, provider-supplied error and status text is withheld
from diagnostics (a service can echo header values); the job ID and failure stage are still reported.

No live account access is proven by local fixture tests. Verify a provider with an explicitly approved
generation using your own account or local server before relying on it.

## API references

- [Gemini image generation](https://ai.google.dev/gemini-api/docs/image-generation) and
  [generateContent reference](https://ai.google.dev/api/generate-content)
- [Imagen on Vertex](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/image/generate-images)
- [BFL generation and polling](https://docs.bfl.ai/quick_start/generating_images)
- [fal queue lifecycle](https://docs.fal.ai/model-apis/model-endpoints/queue)
- [Replicate HTTP API](https://replicate.com/docs/reference/http)
- [ComfyUI API example](https://github.com/comfyanonymous/ComfyUI/blob/master/script_examples/websockets_api_example.py)
