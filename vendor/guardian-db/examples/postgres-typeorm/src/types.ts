/**
 * Domain types and interfaces for the example project.
 *
 * These are plain, framework-agnostic TypeScript shapes — the vocabulary the
 * app speaks in. The TypeORM entities (`./entities`) persist them and the Zod
 * schemas (`./schema`) validate untrusted input into them. Keeping the domain
 * model separate from both the ORM and the validator is a deliberate choice:
 * each layer can evolve without dragging the others along.
 */

/** A theme preference stored inside the user's `settings` JSONB column. */
export type Theme = "light" | "dark" | "system";

/** Free-form, structured per-user settings (persisted as JSONB). */
export interface UserSettings {
  theme?: Theme;
  [key: string]: unknown;
}

/** Structured post metadata (persisted as JSONB). */
export interface PostMeta {
  tags: string[];
}

/** An organization row as the app reads it. */
export interface OrgRecord {
  id: number;
  name: string;
}

/** A fully-materialized user row as the app reads it. */
export interface UserRecord {
  id: number;
  email: string;
  name: string;
  settings: UserSettings;
  orgId: number | null;
  createdAt: Date;
  updatedAt: Date;
}

/** A fully-materialized post row. */
export interface PostRecord {
  id: string;
  title: string;
  body: string | null;
  meta: PostMeta;
  published: boolean;
  authorId: number;
  createdAt: Date;
}

/** A simple paginated result wrapper used by the read examples. */
export interface Page<T> {
  items: T[];
  total: number;
  page: number;
  pageSize: number;
}
