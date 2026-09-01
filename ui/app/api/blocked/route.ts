import { NextResponse } from "next/server";

// Target of the next.config rewrite that blocks public access to the node's ZK
// preview endpoints. Those are server-to-server only (membership-verified by
// /api/preview-unlock + /api/zk-enroll), so any browser hit here is rejected.
function blocked() {
  return NextResponse.json({ error: "not available" }, { status: 403 });
}

export const GET = blocked;
export const POST = blocked;
export const PUT = blocked;
export const DELETE = blocked;
export const PATCH = blocked;
