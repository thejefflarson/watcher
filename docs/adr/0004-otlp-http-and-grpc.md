# 0004. Accept OTLP over both HTTP and gRPC, sharing storage code

- Status: Accepted
- Date: 2026-05-30

## Context

OTLP/HTTP (protobuf, `:4318`) is the simplest receiver and what Traefik and many
SDKs use. But the default transport for a lot of OTel SDKs and the Collector is
OTLP/**gRPC** (`:4317`). Supporting only HTTP makes watcher a not-quite-drop-in
endpoint.

## Decision

We will accept OTLP over **both** HTTP (`:4318`, axum) and gRPC (`:4317`, tonic),
running both servers concurrently. The decode-and-store logic lives in
transport-agnostic `otlp::store_{traces,logs,metrics}` functions that both
transports call.

## Consequences

- watcher is a genuine drop-in `OTEL_EXPORTER_OTLP_ENDPOINT` for either transport.
- One place to change storage behavior; the gRPC and HTTP layers stay thin.
- Two listeners to run and (optionally) auth. gRPC over Traefik needs h2c, so for
  now gRPC ingress is in-cluster only; HTTP is what we expose publicly.
