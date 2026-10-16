# Model embeddings for semantic memory

Semantic memory ranks facts by keyword search and, when embeddings are on, by cosine similarity
over stored vectors. Facts are fused with reciprocal-rank fusion. Without embeddings, recall is
keyword-only and nothing leaves the machine.

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
