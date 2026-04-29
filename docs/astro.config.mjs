import { defineConfig } from "astro/config";
import sitemap from "@astrojs/sitemap";
import rehypeSlug from "rehype-slug";
import rehypeAutolinkHeadings from "rehype-autolink-headings";
import rehypeExternalLinks from "rehype-external-links";
import { rewriteInternalLinks } from "./plugins/rewrite-internal-links.mjs";
import ocyncLight from "./src/themes/ocync-light.json";
import ocyncDark from "./src/themes/ocync-dark.json";

const isProd = process.env.NODE_ENV === "production";
const base = isProd ? "/ocync/" : "/";

export default defineConfig({
  site: "https://clowdhaus.github.io",
  base,
  trailingSlash: "never",
  integrations: [sitemap()],
  markdown: {
    remarkPlugins: [
      [rewriteInternalLinks, { base }],
    ],
    rehypePlugins: [
      rehypeSlug,
      [rehypeAutolinkHeadings, {
        behavior: "append",
        properties: { class: "heading-anchor" },
        content: {
          type: "element",
          tagName: "span",
          properties: { class: "icon icon-link" },
          children: [],
        },
      }],
      [rehypeExternalLinks, { rel: ["noopener"], target: false }],
    ],
    shikiConfig: {
      themes: {
        light: ocyncLight,
        dark: ocyncDark,
      },
    },
  },
});
