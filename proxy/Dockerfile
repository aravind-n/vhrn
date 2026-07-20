# Build the egress proxy as a static binary and ship it in scratch — no shell,
# no userland, minimal CVE surface for the component that enforces the boundary.
FROM golang:1.23 AS build
WORKDIR /src
# No third-party modules, so go.mod alone is enough — there is no go.sum to fetch.
COPY go.mod ./
COPY main.go ./
COPY egress ./egress
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /vhrn-proxy .

FROM scratch
COPY --from=build /vhrn-proxy /vhrn-proxy
# Unprivileged; the proxy binds :8080 and needs no root.
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/vhrn-proxy"]
