// Rewrite cross-page markdown links into root-absolute URLs prefixed with the
// Astro `base`. This guarantees that links resolve identically regardless of
// whether the host serves the page at the canonical URL (no trailing slash)
// or at the directory variant (with trailing slash) -- a distinction GitHub
// Pages erases by serving both variants of every directory page.
//
// Convention for cross-page links in markdown source:
//
//   `/some/page`         -- root-absolute slug; rewritten to `${base}some/page`
//   `/some/page#anchor`  -- same, with fragment preserved
//   `#anchor`            -- in-page anchor, left alone
//   `https://...`,
//   `mailto:`, `tel:`    -- left alone
//
// Relative forms (`./foo`, `../foo`) are intentionally rejected at build time
// so that authors don't introduce links whose resolution depends on whether
// the URL has a trailing slash.

import { visit } from "unist-util-visit";

const SCHEME_RE = /^[a-z][a-z0-9+.\-]*:/i;

export function rewriteInternalLinks({ base } = {}) {
  if (!base) throw new Error("rewriteInternalLinks: `base` is required");
  const norm = base.endsWith("/") ? base : `${base}/`;

  return function transformer(tree, file) {
    visit(tree, "link", (node) => {
      const url = node.url;
      if (!url) return;
      if (SCHEME_RE.test(url)) return;     // mailto:, https:, etc.
      if (url.startsWith("//")) return;    // protocol-relative
      if (url.startsWith("#")) return;     // in-page anchor

      if (url.startsWith("./") || url.startsWith("../")) {
        const where = file?.path ? `${file.path}: ` : "";
        throw new Error(
          `${where}relative markdown link "${url}" is not allowed; ` +
          `use a root-absolute slug like "/some/page" instead. ` +
          `Relative links break under trailing-slash URLs that GitHub Pages serves.`
        );
      }

      if (url.startsWith("/")) {
        // Root-absolute content slug: /foo/bar -> ${base}foo/bar
        let tail = url.slice(1);
        if (tail.endsWith(".md")) tail = tail.slice(0, -3);
        const hashIdx = tail.indexOf("#");
        if (hashIdx !== -1) {
          const slug = tail.slice(0, hashIdx).replace(/\/$/, "");
          const frag = tail.slice(hashIdx);
          node.url = `${norm}${slug}${frag}`;
        } else {
          node.url = `${norm}${tail.replace(/\/$/, "")}`;
        }
      }
      // Anything else (bare strings without a scheme or leading slash) is left
      // alone -- e.g. authors writing inline `<file>` references.
    });
  };
}
