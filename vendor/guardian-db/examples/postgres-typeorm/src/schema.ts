/**
 * Schema creation with Zod.
 *
 * Zod is the single source of truth for the *shape of untrusted input*. Each
 * write in `./crud.ts` runs its payload through one of these schemas first, so
 * only well-formed data ever reaches the database — and the validated value is
 * exactly the shape the TypeORM entity expects.
 *
 * The input DTO types are *derived from* the schemas with `z.infer`, so the
 * types and the runtime validation can never drift apart.
 */

import { z } from "zod";
import type { Theme, UserSettings } from "./types";

const THEMES = ["light", "dark", "system"] as const;

/** Compile-time guarantee that the Zod enum and the `Theme` union agree. */
type _ThemeMatches = Theme extends (typeof THEMES)[number] ? true : never;

/** The `theme` value inside a user's settings. */
export const ThemeSchema = z.enum(THEMES);

/** Per-user settings (the JSONB `settings` column): a known `theme` plus any
 *  extra keys the app wants to stash. */
export const UserSettingsSchema: z.ZodType<UserSettings> = z
  .object({ theme: ThemeSchema.optional() })
  .catchall(z.unknown());

/** Structured post metadata (the JSONB `meta` column). */
export const PostMetaSchema = z.object({
  tags: z.array(z.string().min(1)).default([]),
});

/** Validate raw input for creating a user. */
export const CreateUserSchema = z.object({
  email: z.email().max(160),
  name: z.string().trim().min(1).max(200),
  settings: UserSettingsSchema.default({}),
  orgId: z.number().int().positive().nullable().default(null),
});

/**
 * Validate a user *patch*. Every field is optional and unknown keys are
 * rejected (`.strict()`), so a typo in an update payload fails loudly instead of
 * silently doing nothing. `orgId` is intentionally excluded — relink an org via
 * the relation, not a raw column write.
 */
export const UpdateUserSchema = z
  .object({
    email: z.email().max(160),
    name: z.string().trim().min(1).max(200),
    settings: UserSettingsSchema,
  })
  .partial()
  .strict();

/** Validate raw input for creating a post. */
export const CreatePostSchema = z.object({
  title: z.string().trim().min(1).max(300),
  body: z.string().nullable().default(null),
  meta: PostMetaSchema.default({ tags: [] }),
  published: z.boolean().default(false),
  authorId: z.number().int().positive(),
});

// Input DTOs (what a caller provides — schema defaults are optional here)...
export type CreateUserInput = z.input<typeof CreateUserSchema>;
export type UpdateUserInput = z.input<typeof UpdateUserSchema>;
export type CreatePostInput = z.input<typeof CreatePostSchema>;

// ...and the validated values (defaults applied — what gets persisted).
export type CreateUser = z.output<typeof CreateUserSchema>;
export type UpdateUser = z.output<typeof UpdateUserSchema>;
export type CreatePost = z.output<typeof CreatePostSchema>;

/** Render a `ZodError` as a compact `field: message; …` string for logs. */
export function formatIssues(err: z.ZodError): string {
  return err.issues.map((i) => `${i.path.join(".") || "(root)"}: ${i.message}`).join("; ");
}
