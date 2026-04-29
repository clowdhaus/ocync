# docs

Astro static site, deployed to GitHub Pages at <https://clowdhaus.github.io/ocync/>.

## Cross-page link convention

All internal markdown links use root-absolute slug paths. The remark plugin `plugins/rewrite-internal-links.mjs` prepends the Astro `base` and rejects relative paths at build time.

```markdown
[Helm chart](/helm)                  -> /ocync/helm
[ECR auth](/registries/ecr#auth)     -> /ocync/registries/ecr#auth
[Schema](/config.schema.json)        -> /ocync/config.schema.json
[Helm](./helm)                       build error
[Helm](../helm)                      build error
```

The reason is GitHub Pages serves both `/ocync/foo` and `/ocync/foo/` from the same `dist/foo/index.html`. Relative `href`s like `./bar` resolve to `/ocync/foo/bar` (404) under the trailing-slash variant. Root-absolute hrefs are immune to the document URL.

## Verifying links

Status checks on canonical URLs are not enough. To catch the trailing-slash class, resolve every `<a href>` against both the canonical and the trailing-slash form of the document URL and assert the resolved paths match. Pure `#fragment` hrefs are exempt.

```python
canon = urlparse(urljoin(doc_url,        href)).path
slash = urlparse(urljoin(doc_url + "/",  href)).path
assert canon == slash
```

`urllib.parse.urljoin` canonicalizes before joining when called from a slash-free document URL, which hides the bug. The two-form check is the actual reproduction.

When the user reports a specific dead URL, fetch that exact URL first and reproduce its failing href. Do not re-run a crawler that already passed -- it has a blind spot, or it would not have passed.

## Build commands

```bash
npm install                           # once
npm run dev                           # base = /, http://localhost:4321
NODE_ENV=production npm run build     # base = /ocync/, output in dist/
npm run preview                       # serve dist/ at http://localhost:4321/ocync/
```
