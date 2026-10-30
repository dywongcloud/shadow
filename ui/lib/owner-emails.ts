import "server-only";

/** The configured platform owners: HIVE_OWNER_EMAIL is a comma-separated list, lowercased and trimmed with empty entries dropped. */
export function ownerEmails(): string[] {
  return (process.env.HIVE_OWNER_EMAIL || "")
    .toLowerCase()
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

export function isOwnerEmail(email: string): boolean {
  return !!email && ownerEmails().includes(email.toLowerCase());
}

/** One owner address for a backend mint claim (the backend matches the claim against its `admin_emails` case-insensitively), or "" when none is configured. */
export function primaryOwnerEmail(): string {
  return ownerEmails()[0] ?? "";
}
