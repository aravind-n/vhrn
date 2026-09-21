FROM rust:1.98-alpine3.23 AS build

RUN apk add --no-cache binutils build-base

WORKDIR /workspace

COPY Cargo.toml Cargo.lock ./
# Cargo validates every member manifest and the shared lockfile before selecting `-p`.
COPY cli/Cargo.toml cli/Cargo.toml
RUN mkdir -p cli/src && touch cli/src/lib.rs
COPY proxy/Cargo.toml proxy/Cargo.toml
COPY proxy/src proxy/src

RUN cargo build --release --locked -p vhrn-proxy --bin vhrn-proxy \
    && strip target/release/vhrn-proxy \
    && readelf -lW target/release/vhrn-proxy > /tmp/program-headers \
    && ! grep -q 'INTERP' /tmp/program-headers \
    && readelf -dW target/release/vhrn-proxy > /tmp/dynamic-section \
    && ! grep -q '(NEEDED)' /tmp/dynamic-section

FROM scratch

COPY --from=build /workspace/target/release/vhrn-proxy /vhrn-proxy

USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/vhrn-proxy"]
