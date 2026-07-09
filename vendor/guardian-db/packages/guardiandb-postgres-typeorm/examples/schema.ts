// Schema creation with Zod.
//
// Zod is the source of truth for the shape of untrusted input. Each write in
// `./crud.ts` validates its payload here first, and the input DTO types are
// derived from the schemas with `z.infer` so types and validation never drift.

import { z } from "zod";
import type { Theme, UserSettings } from "./types";

const THEMES = ["light", "dark", "system"] as const;

/** Compile-time guarantee that the Zod enum and the `Theme` union agree. */
type _ThemeMatches = Theme extends (typeof THEMES)[number] ? true : never;

/** Per-user settings (the JSONB `settings` column). */
export const UserSettingsSchema: z.ZodType<UserSettings> = z
  .object({ theme: z.enum(THEMES).optional() })
  .catchall(z.unknown());

/** Validate raw input for creating a user. */
export const CreateUserSchema = z.object({
  email: z.email().max(160),
  name: z.string().trim().min(1).max(200),
  settings: UserSettingsSchema.default({}),
});

/** Validate a user patch: every field optional, unknown keys rejected. */
export const UpdateUserSchema = CreateUserSchema.partial().strict();

/** Validate raw input for creating a post. */
export const CreatePostSchema = z.object({
  title: z.string().trim().min(1).max(300),
  body: z.string().nullable().default(null),
  published: z.boolean().default(false),
  authorId: z.number().int().positive(),
});

// Input DTOs (what a caller provides) ...
export type CreateUserInput = z.input<typeof CreateUserSchema>;
export type UpdateUserInput = z.input<typeof UpdateUserSchema>;
export type CreatePostInput = z.input<typeof CreatePostSchema>;

// ... and the validated values (defaults applied — what gets persisted).
export type CreateUser = z.output<typeof CreateUserSchema>;
export type UpdateUser = z.output<typeof UpdateUserSchema>;
export type CreatePost = z.output<typeof CreatePostSchema>;

/** Render a `ZodError` as a compact `field: message; …` string for logs. */
export function formatIssues(err: z.ZodError): string {
  return err.issues.map((i) => `${i.path.join(".") || "(root)"}: ${i.message}`).join("; ");
}
