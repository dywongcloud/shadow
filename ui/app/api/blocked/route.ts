import { NextResponse } from "next/server";

/** Target of the next.config rewrite that blocks public access to the node's ZK preview endpoints (server-to-server only: `/api/preview-unlock` and `/api/zk-enroll` verify
 *  membership), so every browser hit is rejected. */
function blocked() {
  return NextResponse.json({ error: "not available" }, { status: 403 });
}

export const GET = blocked;
export const POST = blocked;
export const PUT = blocked;
export const DELETE = blocked;
export const PATCH = blocked;
