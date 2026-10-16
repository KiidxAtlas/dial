# Model embeddings for semantic memory

Semantic memory ranks facts by keyword search and, when embeddings are on, by cosine similarity
over stored vectors. Facts are fused with reciprocal-rank fusion. Without embeddings, recall is
keyword-only and nothing leaves the machine.

## Set up a local embedding model

Dial can pull a local embedding model through Ollama and assign it to semantic memory in one step.
Nothing downloads until you ask for it.

- Settings → Semantic memory → Embedding model: press Ctrl+L to see the plan, then Ctrl+L again to
  start. The row offers `nomic-embed-text` until an embedding model is configured.
- `/models`, then **Get models** and its Embedding shelf (Tab to reach it): lists every embedding
  model discovery found (Enter assigns a served one to the `embedding` role), then the curated
  catalog. Enter on a curated model sets it up exactly as `/setup embeddings:<model> yes` does.
  Nothing is pulled until Enter.
- `/setup embeddings` lists the curated catalog. `/setup embeddings:<model>` previews the plan and
  `/setup embeddings:<model> yes` runs it in the background. Any other Ollama embedding model name
  also works.
- `dial doctor --fix embeddings:<model>` previews and `--yes` runs the same setup from the shell.

The setup uses the Ollama endpoint that `ollama` resolves to in config: the default local server, or a
configured `providers.ollama` base URL with its headers and key (a trailing `/v1` is dropped). It starts
Ollama if the program is installed but not running, and installs it only for the local default. If
Ollama is missing, it is installed through the dependency registry (Homebrew where available) when the
registry knows how; otherwise the setup stops and shows the download page. A remote server must already
be running; the setup stops with a message when it is not reachable. It then pulls the model with
Ollama's `/api/pull`, which streams progress and can be cancelled; Ollama checks each layer's digest.
Only after the pull succeeds does Dial save the config:

- `model_roles.embedding` is replaced as a whole with `provider = "ollama"`, `model = "<model>"`,
  using the same selection writer as the main model. Endpoint and authentication settings come
  from the selected Ollama provider, not the previous embedding role. Stale role-specific URL,
  key variable, custom headers, account and protocol settings are cleared. With the default
  Ollama provider this uses the local server and native `/api/embed`.
- `memory.semantic.embedding` becomes `on` if it was `off`. `auto` and `on` are left as they are. If a
  project setting still sets it to `off`, the setup says so instead of reporting `on`.
- The explicit `embedding_base_url` / `embedding_model` overrides are not changed. They are
  OpenAI-compatible and still win over the role when both are set; clear them to use the role.
- `memory.backend` is not changed. Semantic recall needs `memory.backend = "semantic"`; select it
  in settings when the setup reports that it is not selected.

Curated models (Ollama library, sizes rounded; dimensions from each model card):

| Model | Dims | Download | Notes |
| --- | --- | --- | --- |
| `nomic-embed-text` | 768 | 274 MB | General English, 8k context. Default. |
| `all-minilm` | 384 | 46 MB | Smallest and fastest; English only. |
| `mxbai-embed-large` | 1024 | 670 MB | Larger English embedder. |
| `embeddinggemma` | 768 | 622 MB | About 300M multilingual, Gemma 3 based. |
| `bge-m3` | 1024 | 1.2 GB | Multilingual, long inputs. |
| `snowflake-arctic-embed2` | 1024 | 1.2 GB | Multilingual retrieval, about 568M. |

Dial does not run embedding inference itself. Ollama serves the model; an MLX or LM Studio
embedding checkpoint needs an embeddings endpoint and the override or role settings above.

## Choosing the endpoint

`memory.semantic.embedding` is one of:

- `off`: recall is keyword-only. Neither the assigned role nor the explicit overrides are
  resolved, so an unusable role or a partial override never fails memory.
- `auto`: use the assigned `embedding` model role (Alt+M). If it is unusable, or the explicit
  overrides are incomplete, recall degrades to keywords and the reason is shown in status.
- `on`: requires a usable role or a complete override pair; otherwise memory reports the error.

Explicit overrides (`embedding_base_url` and `embedding_model`, both required) win over the role.
They are always OpenAI-compatible, use HTTPS except on loopback, and take the key from
`embedding_api_key_env`.

For example, assign an installed Ollama embedding model using its native API:

```toml
[model_roles.embedding]
provider = "ollama"
model = "embeddinggemma"
```

Or assign an OpenAI embedding model (requires the provider's API credential):

```toml
[model_roles.embedding]
provider = "openai"
model = "text-embedding-3-small"
```

These are alternatives, not two tables to paste together. They configure the embedding role;
semantic recall also requires the semantic memory backend to be selected. A role can be configured
directly even when a server's model list omits purpose metadata and cannot offer it in the embedding picker.

## Wire formats

The role's provider protocol selects the request format. Shapes follow the public
[Ollama `/api/embed`](https://docs.ollama.com/api/embed) and
[Gemini `batchEmbedContents`](https://ai.google.dev/api/embeddings) references. The tests use
loopback fixtures; they do not establish live provider availability or model quality.

| Role protocol | Request | Response |
| --- | --- | --- |
| `chat-completions` (OpenAI, LM Studio, vLLM, Ollama `/v1`) | `POST {base}/embeddings` with `{model, input}` | `data[].embedding`, ordered by `index` (missing index means array order) |
| `ollama` (native) | `POST {base}/api/embed` with `{model, input, truncate:false}` | `embeddings[]`, in input order |
| `gemini` (native) | `POST {base}/models/{id}:batchEmbedContents`, `x-goog-api-key` header, one `requests[]` entry per input | `embeddings[].values`, in input order |

URL rules: a bare host gets `/v1` for OpenAI-compatible routes, which matches chat. Ollama native
always uses the server root (a trailing `/v1` or `/api/embed` is removed). A Gemini base without a
path gets `/v1beta`. Gemini model ids may carry a `models/` prefix but may contain only letters,
digits, `-`, `_` and `.`, so no path can be injected.

Any other role protocol (for example Anthropic) is reported as unsupported for embeddings, and
the message names the protocol. Choose a model served over one of the three formats above.

## Headers, credentials and errors

- Custom headers from the role's endpoint are sent after validation. Reserved names (authorization,
  API keys, content type, provider account routing and similar) are rejected before any request.
  Explicit overrides carry no headers.
- The bearer token is sent as `Authorization`, except Gemini, which uses `x-goog-api-key`.
- Non-2xx responses report the HTTP status and the provider's error message. The bearer token and
  env-sourced header values are removed from that message.
- Responses are rejected when a vector holds a non-number or a value outside the f32 range, when an
  index is not a non-negative integer below the input count or is repeated, when counts differ from
  the inputs, or when vectors differ in length.

## Limits and gaps

- No live provider was contacted when this was written. Native Ollama and Gemini support rests on
  the fixture tests and the documented request shapes.
- Explicit overrides cannot select a native protocol yet. Use the role for Ollama or Gemini.
- Role routes honour the chat endpoint's insecure-HTTP opt-in. Overrides require HTTPS, except on loopback.
- Gemini: only `batchEmbedContents` is used. Task types and output dimensionality are not sent.
- Ollama: `truncate:false` means overlong input fails instead of being cut silently.
- Setup's Ollama pull is cancelled between progress frames. A stream that stops sending frames is not
  interrupted until the connection fails.
