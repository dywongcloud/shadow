// Server-only facade over the pure artifact builder. The implementation lives in
// `gitops-artifacts.ts` (isomorphic — also used by the client-side local provider).
// Existing server consumers (gitops-server.ts, the gitops API routes) import from
// here so the `server-only` guard keeps secret-adjacent server code off the client.
import "server-only";

export * from "./gitops-artifacts";
