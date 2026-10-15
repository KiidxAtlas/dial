FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates bubblewrap \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 dial
COPY --from=build /src/target/release/dial /usr/local/bin/dial
COPY LICENSE /usr/share/doc/dial/LICENSE
COPY THIRD_PARTY_NOTICES /usr/share/doc/dial/THIRD_PARTY_NOTICES
COPY src/agent/snapcompact/fonts/Silver.LICENSE /usr/share/doc/dial/Silver.LICENSE
COPY src/agent/snapcompact/fonts/Unscii.LICENSE /usr/share/doc/dial/Unscii.LICENSE
USER dial
WORKDIR /workspace
ENTRYPOINT ["dial"]
