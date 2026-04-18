import { defineCollection, z } from "astro:content";
import { glob } from "astro/loaders";

const docs = defineCollection({
  loader: glob({ pattern: "**/*.md", base: "src/content" }),
  schema: z.object({
    title: z.string(),
    description: z.string(),
    order: z.number().optional(), // Used for future data-driven sidebar ordering
  }),
});

export const collections = { docs };
