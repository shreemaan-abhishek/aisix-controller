# syntax=docker/dockerfile:1
FROM rust:1.93-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/aisix-controller /aisix-controller

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /aisix-controller /aisix-controller
USER 65532:65532
ENTRYPOINT ["/aisix-controller"]
CMD ["run"]
