# 0022. Accept an internal Kubernetes ServiceAccount-token on `/api`

- Status: Accepted
- Date: 2026-11-07
- Related: [0013](0013-auth-at-the-edge.md) (edge auth + origin verify), [0019](0019-mcp-auth-cloudflare-access-oidc.md) (the shared `access_jwt` verifier)

## Context

[0013](0013-auth-at-the-edge.md) gates the public read surface (UI shell + `/api`) with
Cloudflare Access at the edge and re-verifies the forwarded `Cf-Access-Jwt-Assertion`
JWT at the origin. That is the right gate for a *browser* reaching watcher through the
Cloudflare tunnel, but it is the wrong gate for an *in-cluster* caller.

The cluster repo's **fixer-dashboard** runs in the same Kubernetes cluster as watcher and
needs to read `/api`. Routing it out to the Cloudflare edge and back to obtain an Access
assertion is absurd for a Service-to-Service call on the pod network: it is slower, it
couples an internal control path to the public edge, and it has no browser to carry the
Access cookie. What the fixer-dashboard *does* already have is a projected Kubernetes
ServiceAccount token.

Those tokens are OIDC JWTs the cluster itself issues and can verify:

- issuer `https://kubernetes.default.svc.cluster.local`, signed **RS256**;
- JWKS published in-pod at `https://kubernetes.default.svc.cluster.local/openid/v1/jwks`;
- a projected token carries `aud=["watcher"]` (the audience we require), `exp`, and
  `sub="system:serviceaccount:<namespace>:<name>"`.

Reading that JWKS is itself authenticated: the `system:service-account-issuer-discovery`
ClusterRole is bound to `system:serviceaccounts`, so watcher's own SA already qualifies.
The fetch must present watcher's own projected token as `Authorization: Bearer` and trust
the cluster CA.

## Decision

We will accept an internal ServiceAccount-token JWT on `/api` **in addition to** the
Cloudflare Access assertion. This is pure addition — the edge/Access path is unchanged.

- **Reuse the existing verifier.** `access_jwt::Verifier` already checks RS256 signature
  + `iss` + `aud` + `exp` and is issuer/audience-agnostic. A new `Verifier::for_cluster`
  constructor points it at the cluster issuer/JWKS, trusts the cluster CA
  (`reqwest::Certificate::from_pem`), and reads watcher's SA token **fresh on every JWKS
  fetch** (projected tokens rotate) to authenticate the discovery request.
- **Require a `sub` allowlist.** A verified internal token is admitted only when its
  `sub` is in `WATCHER_INTERNAL_ALLOWED_SUBS` (e.g.
  `system:serviceaccount:fixer-dashboard:fixer-dashboard`). A valid token with a
  non-allowlisted `sub` is a hard `401` — a presented-but-unauthorized identity is a
  reject, never a fall-through. If the internal verifier is configured but the allowlist
  is empty, internal auth is treated as **off** (logged at ERROR) — fail closed.
- **One guard, two paths, tried in order.** The `/api`+UI guard first tries the internal
  path when an `Authorization: Bearer` token is present; on verify failure it **falls
  through** to the edge path (it never fails *open* on the internal path). The edge path
  is exactly as before: `Cf-Access-Jwt-Assertion` `Valid`/`KeysUnavailable` admit (edge
  fail-open preserved), `Missing`/`Invalid` reject.

### Posture: fail-closed internal, fail-open edge

The edge path remains defense-in-depth behind Cloudflare Access, so a cold-cache cluster
**JWKS outage keeps its fail-open behavior there** (the edge is still the gate). The
internal path fails **closed**: an internal caller sends no `Cf-Access-Jwt-Assertion`, so
when its token can't be verified it falls through to the edge path, hits `Missing`, and
gets `401`. A cluster-JWKS outage therefore admits no unverified internal caller.

### Configuration (env vars)

| Variable | Default | Meaning |
| --- | --- | --- |
| `WATCHER_INTERNAL_AUD` | *(unset ⇒ feature off)* | Required audience; enables the feature when set and non-empty. |
| `WATCHER_INTERNAL_ALLOWED_SUBS` | *(empty ⇒ internal off)* | Comma-separated exact `sub` allowlist. |
| `WATCHER_INTERNAL_ISSUER` | `https://kubernetes.default.svc.cluster.local` | Expected `iss`. |
| `WATCHER_INTERNAL_JWKS_URL` | `https://kubernetes.default.svc.cluster.local/openid/v1/jwks` | JWKS endpoint. |
| `WATCHER_INTERNAL_TOKEN_PATH` | `/var/run/secrets/kubernetes.io/serviceaccount/token` | watcher's own SA token, read fresh per fetch. |
| `WATCHER_INTERNAL_CA_PATH` | `/var/run/secrets/kubernetes.io/serviceaccount/ca.crt` | Cluster CA trusted for the JWKS fetch. |

## Consequences

- The fixer-dashboard (and any future in-cluster `/api` client) authenticates with its
  own SA token — no Cloudflare round-trip, scoped to a named identity. This ADR is
  consumed by the cluster repo's fixer-dashboard, which sets the allowlist to its own SA.
- The attack surface stays narrow: RS256 only (no alg-confusion regression), the exact
  cluster `iss` and `watcher` `aud`, a non-expired token, and an explicit `sub`
  allowlist. Token contents are never logged — only the `sub` on a reject.
- The edge path is untouched; existing Access tests and behavior stand. The cost is a
  second verifier and a slightly more involved guard (two ordered paths instead of one),
  plus the operational duty to keep the allowlist correct — an empty or wrong allowlist
  fails closed, which is the safe direction.
- Unset `WATCHER_INTERNAL_AUD` leaves the feature entirely out of the wiring, so local
  dev and non-cluster deploys are unchanged.
