import { defineConfig } from "astro/config";
import sitemap from "@astrojs/sitemap";
import rehypeSlug from "rehype-slug";
import rehypeAutolinkHeadings from "rehype-autolink-headings";
import rehypeExternalLinks from "rehype-external-links";
import ocyncLight from "./src/themes/ocync-light.json";
import ocyncDark from "./src/themes/ocync-dark.json";

const isProd = process.env.NODE_ENV === "production";

export default defineConfig({
  site: "https://clowdhaus.github.io",
  base: isProd ? "/ocync/" : "/",
  trailingSlash: "never",
  integrations: [sitemap()],
  markdown: {
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
