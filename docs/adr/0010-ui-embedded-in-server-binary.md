# 0010. Embed the UI in the server binary (drop nginx)

- Status: Accepted
- Date: 2026-05-30
- Supersedes: [0006](0006-single-origin-ui.md)

## Context

[0006](0006-single-origin-ui.md) served the SPA from a separate nginx container and
relied on the Traefik IngressRoute to path-split `/api` + `/v1` to the server and
everything else to nginx. That split is the *only* thing that kept the UI's
same-origin `/api` calls working — and it lived in three places (nginx config, the
IngressRoute, `VITE_API_BASE`).

When the cluster ran with `ingressRoute.enabled: false` (in-cluster/port-forward
access, no Traefik route), nothing peeled off `/api`. The UI's `/api/*` calls hit
nginx, whose SPA fallback (`try_files … /index.html`) returned **HTML with status
200**. The client's `res.ok` check passed, then `JSON.parse` choked on `<` —
*"unexpected character at line 1 column 1."*

## Decision

The Rust server serves the built UI itself. `ui/dist` is compiled into the binary
with `rust-embed`; axum routes `/api`, `/v1`, and `/healthz` to handlers and falls
back to the embedded SPA (with `index.html` for unknown client routes). There is
**one image, one deployment, one origin**. nginx, the separate `watcher-ui` image,
and the IngressRoute path-split are gone.

## Consequences

- The same-origin `/api` contract holds with or without an ingress — the failure
  mode that produced the JSON.parse error cannot recur (regression-tested in
  `ui_fallback_does_not_shadow_api`).
- One fewer image to build/push and one fewer pod to run — lighter on a Pi.
- `cargo build` now needs `ui/dist` to exist; `build.rs` creates an empty
  placeholder so a server-only build still compiles (it serves a "UI not built"
  notice), and the Docker/CI build runs `npm run build` before `cargo build`.
- Build context for the image is the repo root (it needs both `server/` and `ui/`).
- Local dev is unchanged: Vite on `:5173` calls `http://localhost:4318` with the
  server's permissive CORS layer.
