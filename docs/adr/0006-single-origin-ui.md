# 0006. Single-origin deployment, path-split by Traefik

- Status: Accepted
- Date: 2026-05-30

## Context

watcher is a server (OTLP ingest + JSON API) plus a static SPA. We could serve them
on separate hostnames (CORS, two ingresses) or co-locate them. The SPA needs to
call the API; baking an absolute API URL into the build is brittle.

## Decision

The UI and API will live behind **one hostname**. The Traefik IngressRoute
path-splits: `/v1` + `/api` + `/healthz` → the server, everything else → the UI
(nginx). The UI image is built with `VITE_API_BASE=""` so it calls the API
**same-origin** (relative `/api`, `/v1`). In local dev the UI defaults to
`http://localhost:4318`.

## Consequences

- No CORS in production, one cert, one tunnel. The UI is portable across
  deployments (no host baked in).
- The server keeps a permissive CORS layer so local dev (`:5173` → `:4318`) works.
- gRPC (`:4317`) is not path-routable over the same HTTP ingress; it stays
  in-cluster (see [0004](0004-otlp-http-and-grpc.md)).
