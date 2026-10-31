FROM rust:1.99-bookworm AS build
# The build context excludes .git; release builds require the source commit explicitly.
ARG DIAL_BUILD_REVISION
ARG DIAL_RELEASE_PUBLIC_KEY
WORKDIR /src
COPY . .
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
USER dial
# Dial's private state (settings, sessions, credentials). Mount a volume here to keep it.
ENV DIAL_HOME=/home/dial/.dial
VOLUME /home/dial/.dial
WORKDIR /workspace
ENTRYPOINT ["dial"]
