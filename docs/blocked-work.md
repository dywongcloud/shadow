# Blocked / carried work (re-homed from .gm/prd.yml)

These rows were re-homed out of the active gm PRD on 2026-07-29 with the
owner's explicit approval, to close a documented rs-plugkit gate disagreement:
`residual-scan` refuses while ANY PRD row is pending, while the CONSOLIDATE
gate excuses `blockedBy` rows — so a chain whose remaining rows are all
legitimately blocked can never formally advance. The upstream fix stays filed
below (`plugkit-prd-state-reset-data-loss`,
`residual-scan-consolidate-blocked-rows-deadlock`).

Every row here is REAL, tracked work — carried verbatim, with its blocker,
not abandoned. When a blocker clears (physical access, owner decision,
upstream fix lands, account access granted), move the row back into
`.gm/prd.yml` via `prd-add` and work it.

## plugkit-prd-state-reset-data-loss

**Blocked by:** external; fix lives in rs-plugkit/agentplug, not this repo; recovery done, prevention upstream

PLUGKIT DATA-LOSS BUG, witnessed live this session: the shared agentplug daemon's internal PRD store RESET mid-chain (prd-resolve returned known_ids:[] for ids that were resolvable minutes earlier), and the next prd-add/prd-resolve dispatches made it REWRITE .gm/prd.yml from that near-empty internal state - destroying all 44 rows on disk (13 blocked + pending + completed history). .gm/* is gitignored so no git recovery; recovered by re-adding rows verbatim from session context (all pending rows restored; completed-row history accepted as lost from the file - real witnesses live in .gm/exec-spool/.watcher.log and the git commit messages). Fix belongs in rs-plugkit/agentplug: (a) on state reset/restart, REHYDRATE the PRD store from .gm/prd.yml before serving any prd verb, (b) never serialize a state known to be a subset of the on-disk file (row-count sanity check before overwrite), (c) write prd.yml via atomic temp+rename with a .bak of the prior version.

## agent-recovery-deep-cell-tunnel

**Blocked by:** out-of-reach; platform-side fix (911df6c) shipped and verified correct via source; remaining stall needs the account owner or app-level access; OOM finding needs a dedicated live-repro pass

UPDATE: got the exact kernel OOM evidence: journalctl -k on fc-sanjose: tokio-rt-worker invoked oom-killer ... oom_memcg=/system.slice/hive-node.service ... Killed process 367843 (hive-cloud) total-vm:26132172kB anon-rss:13551208kB - hive-cloud ITSELF grew to ~12.9GB anon before the kernel killed it, same SHAPE as the sj2 incident and the guardian-init-leak already fixed, but not provably the SAME mechanism (that fix was live on this exact binary). Plausible-context triggers (leader restart + 12-node reconvergence + replay/reenqueue bursts + 16+ orphaned-cell-gc entries) recorded but unproven. Recovered, RSS stable 2.29GB/16.5GB cap. Needs a dedicated live heap-profiler-driven repro pass; the remaining shoomoo stall is very likely third-party app internals needing account-owner access.

## recover-verify-agent-e2e

**Blocked by:** out-of-reach; depends on agent-recovery-deep-cell-tunnel

End-to-end verification of the shoomoo agent - not achievable while workflow delivery is gateway-broken.

## home-force-dynamic-to-ppr

**Blocked by:** external; upstream vercel/next.js#85490

Re-investigate replacing the home page force-dynamic with real PPR. Depends on cacheComponents:true, itself blocked on the confirmed upstream Clerk + Next.js Cache Components bug vercel/next.js#85490. Home page stays on its original working force-dynamic until that lands.

## fix-restart-disruption

**Blocked by:** out-of-reach; needs a dedicated pass tracing the fluid-gateway tunnel/cell lifecycle during shutdown - outer-listener half shipped (13ec66f)

REAL PARTIAL FIX SHIPPED (13ec66f) and honestly verified INCOMPLETE: axum_server graceful drain on :443 works exactly as designed (draining->flush 15.0s apart), but a live test proved an in-flight request through the fluid-gateway tunnel to a Firecracker cell still gets NOTHING back when hive-node restarts mid-request (curl EXIT=52 HTTP/1.1, EXIT=16 HTTP/2) even within the grace window. REMAINING: trace what happens to an in-flight fluid-gateway tunnel/lease during process shutdown - does cloud.mesh/cloud.iroh drop early, does the cell connection tear down independent of the outer HTTP grace, is a SEPARATE tunnel-layer drain needed.

## remediate-stabilize-cell

**Blocked by:** out-of-reach; depends on agent-recovery-deep-cell-tunnel

Stabilize the shoomoo cell - RUNTIME_TUNNEL_FAILED persists; further live poking risks harm.

## lax3-daemon-restart

**Blocked by:** external; needs physical access - see lax3-tunnel-reachability

Cannot diagnose/restart fc-lax3 daemon with zero network channel to the machine.

## lax3-tunnel-reachability

**Blocked by:** external; needs physical access to the machine

Both previously-proven access paths to fc-lax3 (ngrok tunnel, local mDNS to Weaves-MacBook-Air.local) are dead; the machine needs someone physically present.

## remediate-verify-agent-responds

**Blocked by:** out-of-reach; depends on agent-recovery-deep-cell-tunnel

Verify the shoomoo Telegram agent actually responds - cannot until the deep recovery lands (0 runs completing).

## lax3-rejoin-witness

**Blocked by:** external; needs physical access - see lax3-tunnel-reachability

Cannot witness a mesh rejoin that has not happened - same physical-access gap.

## residual-scan-consolidate-blocked-rows-deadlock

**Blocked by:** external; rs-plugkit gate disagreement: residual-scan counts blockedBy rows; needs the scan to accept blocked-only pending sets or CONSOLIDATE to accept a fired-but-refused scan

STUCK-LOOP-ESCALATION (gate repeat_count=5, spanning multiple sessions): residual-scan refuses with residual-premature PRD-still-has-items because 12 rows remain pending — but ALL 12 carry legitimate blockedBy (user decisions: EULA, billing write-off; physical access: fc-lax3 x3; upstream: plugkit PRD-store fix, vercel/next.js#85490; account-owner/dedicated-repro: shoomoo agent chain x4) and ready_wave is EMPTY (zero reachable rows, all four hardening rows + budget row resolved this session with live witnesses). CONSOLIDATE then refuses because the residual-scan marker is not set in this stop window — the documented gates-disagree (residual-scan counts blockedBy rows the CONSOLIDATE gate excuses, per resolved row residual-scan-blockedby-deadlock). Chain state otherwise fully converged: worktree clean, HEAD 0f5d6e54ab pushed, CI green (run 30453844136), .ci-validated written with current SHA, fleet rolled 13/13 on the hardened binary, zero reachable residuals.
