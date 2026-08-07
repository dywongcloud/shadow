"use client";

import {
  Database as DbIcon, HardDrive, ListOrdered, Boxes, Zap, Radio, Wifi, FileStack,
} from "lucide-react";
import { Badge } from "@/components/ui";
import { KIND_LABEL, type UnifiedKind } from "./db-model";

/**
 * One presentation table for every database kind, SQLite included. The page and
 * the detail view both read it, so a SQLite row can never drift into looking
 * like a different class of object than a Postgres row.
 */
export const KIND_ICON: Record<UnifiedKind, React.ReactNode> = {
  postgres: <DbIcon className="h-5 w-5" />,
  redis: <Zap className="h-5 w-5" />,
  blob: <HardDrive className="h-5 w-5" />,
  queue: <ListOrdered className="h-5 w-5" />,
  vector: <Boxes className="h-5 w-5" />,
  pubsub: <Radio className="h-5 w-5" />,
  realtime: <Wifi className="h-5 w-5" />,
  sqlite: <FileStack className="h-5 w-5" />,
  browser_sqlite: <FileStack className="h-5 w-5" />,
};

const KIND_TONE = {
  postgres: "blue",
  redis: "red",
  blob: "amber",
  queue: "green",
  vector: "default",
  pubsub: "blue",
  realtime: "green",
  sqlite: "green",
  browser_sqlite: "green",
} as const;

export function KindBadge({ kind }: { kind: UnifiedKind }) {
  return <Badge tone={KIND_TONE[kind] ?? "default"}>{KIND_LABEL[kind] ?? kind}</Badge>;
}
