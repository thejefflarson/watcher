# 0008. Optional bearer-token auth, open by default

- Status: Superseded by [0013](0013-auth-at-the-edge.md)
- Date: 2026-05-30

> **Superseded.** The app-layer bearer tokens were removed in favour of edge auth
> (Cloudflare Access) for the public read surface, with ingest kept in-cluster.
> See [0013](0013-auth-at-the-edge.md).

## Context

watcher runs in-cluster behind no public ingress by default, so its threat model is
mostly trusted. But ingest and the API are distinct surfaces (machines push
telemetry; humans read it) and some deployments will expose one or both.

## Decision

Two **independent, optional** bearer tokens, read from env:

- `WATCHER_INGEST_TOKEN` — guards `/v1/*` (HTTP) and the gRPC services.
- `WATCHER_API_TOKEN` — guards `/api/*`.

When a token is unset, that surface is unauthenticated (backward-compatible,
tests run without tokens). The UI stores an API token in `localStorage` and sends
it as `Authorization: Bearer`.

## Consequences

- Zero-config for the trusted in-cluster case; opt-in hardening when exposed.
- Two tokens means ingest and read can be secured independently.
- It's a shared bearer secret, not per-client identity or OIDC. Fine for v0; a
  real multi-tenant story would need more.
