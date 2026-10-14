"use client";

import type { ReactNode } from "react";
import { useCan } from "@/lib/api";
import { roleHint, roleNote, type TeamAction } from "@/lib/team-role";

/** Wraps a group of controls (a card, a form, a list's buttons) that `action` needs a higher team role for: a role below the matrix floor gets the group read-only (a disabled fieldset disables
 *  every button, input, select and switch inside, footer actions included) with a short tooltip, and `note` adds a line above it; a role at or above the floor, an operator, or an unknown role
 *  (the server decides) renders the children untouched, with no wrapper. */
export function RoleGate({ action, note = false, children }: { action: TeamAction; note?: boolean; children: ReactNode }) {
  const can = useCan();
  if (can(action)) return <>{children}</>;
  return (
    <fieldset disabled title={roleHint(action)} data-role-gate={action} className="m-0 min-w-0 border-0 p-0">
      {note ? <p className="mb-2 text-xs text-muted">{roleNote(action)}</p> : null}
      {children}
    </fieldset>
  );
}
