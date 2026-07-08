// Domain types and interfaces for the package examples.
//
// Plain, framework-agnostic shapes the app reads and writes. The entities
// (`./entities`) persist them; the Zod schemas (`./schema`) validate input into
// them.

/** A theme preference stored in the user's `settings` JSONB column. */
export type Theme = "light" | "dark" | "system";

/** Free-form, structured per-user settings (persisted as JSONB). */
export interface UserSettings {
  theme?: Theme;
  [key: string]: unknown;
}

/** A fully-materialized user row as the app reads it. */
export interface UserRecord {
  id: number;
  email: string;
  name: string;
  settings: UserSettings;
  createdAt: Date;
}

/** A fully-materialized post row. */
export interface PostRecord {
  id: string;
  title: string;
  body: string | null;
  published: boolean;
  authorId: number;
  createdAt: Date;
}

/** A simple result wrapper used by the read example. */
export interface Page<T> {
  items: T[];
  total: number;
}
