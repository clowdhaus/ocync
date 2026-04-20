---
title: Chainguard
description: Using ocync with Chainguard registry, covering per-scope token auth and configuration.
order: 6
---

## Auth

Chainguard (`cgr.dev`) uses OCI token exchange with per-repository scope. Each repository requires its own token. A token scoped to `chainguard/nginx` will 403 when used against `chainguard/python`.

`ocync` handles this with a scope-keyed token cache that maintains separate tokens per repository.

## Rate limits

Chainguard does not impose rate limits on authenticated pulls.

## Example config

```yaml
registries:
  chainguard:
    url: cgr.dev
  ecr:
    url: 123456789012.dkr.ecr.us-east-1.amazonaws.com

defaults:
  source: chainguard
  targets: ecr

mappings:
  - from: chainguard/nginx
    to: nginx
  - from: chainguard/python
    to: python
```
