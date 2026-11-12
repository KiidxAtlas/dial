# Running Dial in Docker

The image runs the Linux build of Dial as the non-root user `dial` (uid 10001). It contains
Dial, Bubblewrap (the Linux command sandbox), Git and CA certificates, and nothing else: no
Python/MLX runtime, so local MLX models are unavailable inside it. Use a hosted provider, a
model server on the host, or Docker Model Runner.

## Build

The build context excludes `.git`, so pass the source revision:

```sh
docker build --build-arg DIAL_BUILD_REVISION="$(git rev-parse HEAD)" -t dial .
```

Release images are published to `ghcr.io/kiidxatlas/dial`; pin a tag or digest.

## Run

```sh
docker run --rm -it \
  -v "$PWD":/workspace \
  -v dial-home:/home/dial/.dial \
  -e ANTHROPIC_API_KEY \
  dial
```

- `-v "$PWD":/workspace` is the project Dial edits. It is the container's working directory.
- `-v dial-home:/home/dial/.dial` keeps settings, sessions and credentials between runs
  (`DIAL_HOME` is set to that path). Without it, state disappears with the container.
- `-e NAME` passes a key from your shell without writing it on the command line. Providers read
  `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `GROQ_API_KEY`,
  `MISTRAL_API_KEY` and `DEEPSEEK_API_KEY`.
- `-it` gives the terminal UI a TTY. Omit it for one-shot use:
  `docker run --rm -v "$PWD":/workspace -e ANTHROPIC_API_KEY dial -p -y "summarize this repo"`.
- On Linux, files created in the mount belong to uid 10001. Add `--user "$(id -u):$(id -g)"`
  to write as yourself, and use a bind mount for state too (`-v "$HOME/.dial-docker":/home/dial/.dial`,
  created beforehand) so the directory is writable by that uid.

### Docker Compose

`docker-compose.yml` preserves Docker's default syscall and `/proc` protections:

```sh
export ANTHROPIC_API_KEY=...        # whichever providers you use
docker compose run --rm dial                              # terminal UI in the current directory
docker compose run --rm dial -p -y "summarize this repo"  # one shot
```

It builds the local checkout when the image is missing (`docker compose build`, with
`DIAL_BUILD_REVISION=$(git rev-parse HEAD)` exported), otherwise uses
`ghcr.io/kiidxatlas/dial:latest` (override with `DIAL_IMAGE`). `DIAL_WORKSPACE` selects another
project directory.

## Sandbox

Commands run unconfined inside the container by default (`sandbox.mode = "off"`). With
`sandbox.mode = "dev"` or `"strict"`, Dial runs commands under Bubblewrap, which needs
unprivileged user namespaces and a fresh `/proc` mount. Docker's default seccomp profile blocks
the first and its masked `/proc` paths block the second, so inside a default container the
sandbox probe fails and command execution is **disabled** with an error; it never falls back to
running unsandboxed. The default recipes retain those container protections. To use the sandbox
inside a container, accepting weaker container isolation, use the separate override:

```sh
docker compose -f docker-compose.yml -f docker-compose.sandbox.yml run --rm dial
```

It sets `seccomp=unconfined` and `systempaths=unconfined` so Bubblewrap can operate without
`--privileged`. Docker users can pass the same two `--security-opt` options explicitly.
Some AppArmor hosts additionally need a reviewed host policy change. These options relax
the container's security boundary; they are not the default or a substitute for Dial's
command sandbox. File, search and web tools work with the default protections.

## Docker Model Runner (Docker AI)

Dial discovers [Docker Model Runner](https://docs.docker.com/ai/model-runner/) as the
`docker-model-runner` provider, next to LM Studio, Ollama and llama.cpp. Its OpenAI-compatible
API needs no key. Pull a model with `docker model pull ai/smollm2`; its models then appear in
`/models` (and `dial models`) with their `docker.io/ai/…` names.

| Where Dial runs | Address Dial uses |
|---|---|
| On the host | `http://127.0.0.1:12434/engines/v1` (enable host-side TCP access in Docker Desktop's Model Runner settings first) |
| In a container (Docker Desktop) | `http://model-runner.docker.internal/engines/v1` |
| In a container (Docker Engine on Linux) | `http://172.17.0.1:12434/engines/v1` |

Set `DOCKER_MODEL_RUNNER_BASE_URL` to change the discovered address. The compose file already sets
it to the container address; with `docker run` add
`-e DOCKER_MODEL_RUNNER_BASE_URL=http://model-runner.docker.internal/engines/v1`. To use it
without discovery, `dial --endpoint <base-url> -m docker.io/ai/smollm2:latest`. Docker's
`*.docker.internal` names count as local hosts, so plain `http` is accepted and requests are
treated as local inference (no proxy, no per-token cost).

Docker Model Runner's llama.cpp backend cannot compile tool grammars with large bounded
repetitions. For the `docker-model-runner` provider, Dial omits string-length and array-length
bounds above 1024 from the outbound tool schemas, including constrained tool-call repair.
The original tool schemas and their runtime validation are unchanged: an out-of-bounds call
is still rejected before execution. Other providers keep their schemas unchanged.
If the server returns `Failed to initialize samplers: failed to parse grammar`, Dial identifies
it as a backend generation-grammar failure rather than a tool execution failure. Use the named
`docker-model-runner` provider for this compatibility policy; a generic `--endpoint` route does
not identify the backend as Docker. Do not disable runtime validation to work around it.

The same container setup reaches other servers on the host: the compose file points
`LM_STUDIO_BASE_URL`, `OLLAMA_BASE_URL` and `LLAMA_CPP_BASE_URL` at `host.docker.internal`. Those
servers must listen on an address the container can reach, not only on the host's loopback.

## Checks

The **Container image** workflow (`.github/workflows/docker.yml`) builds the image, runs
`dial --version` and `dial --help` from it, and validates the compose file. It runs only when the
Dockerfiles, compose files, `Cargo.lock`, the toolchain pin or that workflow change; the release
workflow builds and smoke-tests the published image on every release.
