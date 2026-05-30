# 0005. Dedicated Postgres via the Zalando operator

- Status: Accepted
- Date: 2026-05-30

## Context

The target cluster already runs the Zalando **postgres-operator**. watcher needs a
database. Options: reuse an existing shared cluster (add a `watcher` db/user) or
provision a dedicated one. The operator hands out credentials as a Kubernetes
Secret with split `username` / `password` keys — but the server wants a single
`DATABASE_URL`.

## Decision

The Helm chart will declare a **dedicated** `postgresql` custom resource
(1 instance, small volume) so watcher owns its data and the chart is
self-contained. The server composes `DATABASE_URL` at runtime from the operator's
credential secret using in-order `$(PGPASSWORD)` env-var expansion:

```
postgres://watcher:$(PGPASSWORD)@watcher-db:5432/watcher
```

## Consequences

- Self-contained chart: `helm install` provisions storage with no external steps.
- No secret-templating gymnastics — Kubernetes does the substitution.
- One more Postgres pod (~256 MB) instead of sharing. Acceptable on the homelab;
  reuse a shared cluster later by overriding `postgres.*` values if needed.
- Hard dependency on the postgres-operator being present in the cluster.
