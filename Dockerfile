FROM --platform=$BUILDPLATFORM python:3.15-slim-bookworm AS python-payload
ARG TARGETARCH
WORKDIR /src
COPY scripts/prepare-python.py scripts/runtime_payload.py scripts/
COPY runtimes/python runtimes/python
# For offline staging: --secret id=python_archive,src=/path/to/pinned-target.tar.gz
RUN --mount=type=secret,id=python_archive \
    case "$TARGETARCH" in arm64) arch=aarch64;; amd64) arch=x86_64;; *) exit 1;; esac && \
    set -- && if [ -f /run/secrets/python_archive ]; then set -- --archive /run/secrets/python_archive; fi && \
    python scripts/prepare-python.py --target "$arch-unknown-linux-gnu" --output /payload "$@"

FROM rust:1.100-bookworm AS build
# The build context excludes .git; release builds require the source commit explicitly.
ARG DIAL_BUILD_REVISION
ARG DIAL_RELEASE_PUBLIC_KEY
ARG SOURCE_DATE_EPOCH
WORKDIR /src
COPY . .
COPY --from=python-payload /payload /python-payload
ENV DIAL_BUNDLED_PYTHON_DIR=/python-payload
# Cache mounts keep the registry and incremental artifacts between local rebuilds; the binary is
# copied out because cache mounts are not part of the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p dial \
    && install -D target/release/dial /out/dial

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates bubblewrap git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 dial \
    && install -d -o dial -g dial /workspace /home/dial/.dial
COPY --from=build /out/dial /usr/local/bin/dial
COPY LICENSE /usr/share/doc/dial/LICENSE
COPY THIRD_PARTY_NOTICES /usr/share/doc/dial/THIRD_PARTY_NOTICES
COPY crates/dial-agent/src/agent/compact/snapcompact/fonts/Silver.LICENSE /usr/share/doc/dial/Silver.LICENSE
COPY crates/dial-agent/src/agent/compact/snapcompact/fonts/Unscii.LICENSE /usr/share/doc/dial/Unscii.LICENSE
COPY --from=python-payload /payload/LICENSE.python /usr/share/doc/dial/LICENSE.python
USER dial
# Dial's private state (settings, sessions, credentials). Mount a volume here to keep it.
ENV DIAL_HOME=/home/dial/.dial
VOLUME /home/dial/.dial
WORKDIR /workspace
ENTRYPOINT ["dial"]
