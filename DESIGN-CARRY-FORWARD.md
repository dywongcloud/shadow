# Carry-forward ledger (facts about THIS medium, established by measurement)

Read this file before Step 1 of any later dada run on this project.

| Fact | Measurement that established it | Move it changed |
| --- | --- | --- |
| Tailwind here is 3.4.9, so `min-[380px]:` arbitrary breakpoints ARE generated; no new config entry is needed. | ui/package.json:44 read 2026-11-09, exhaustive regex over the file. | M4/M5 kept the arbitrary breakpoint instead of falling back to only `sm:`. |
| `overflow-x: clip` at ui/app/globals.css:214 is scoped to `html:has(#docs-content)`, NOT global. A "no horizontal page scroll" measurement on /storage is therefore honest, not an artifact of a clipping rule. | globals.css:211-215 read verbatim 2026-11-09. | M3/M5 did not rely on a global clip. |
| `.tap-44` is `min-height:44px !important; min-width:44px !important` under `@media (pointer: coarse), (any-pointer: coarse)` (globals.css:219-224). A width-based utility cannot lower it. | globals.css:219-224 read verbatim. | M1/M3 must keep `tap-44` on any compact control that replaces a hidden crumb. |
| Next `Image` with `width`/`height` props plus a height-only utility needs `w-auto` AND `shrink-0` on the flex item; the intrinsic 274:91 wordmark otherwise absorbs the crumb chain's compression. | ui/components/logo.tsx:4-11 + ui/components/topnav.tsx:101-141 read before the move. | M1. |
| A media query adds no specificity: a later plain rule at equal specificity beats an earlier media-query rule. | carry-forward from the dada skill's own CSS example, confirmed against local globals.css rule order. | M2 requires exhaustive caller enumeration (sub02) rather than trusting the patch. |
| `<Table>` in ui/components/ui.tsx:160-165 already wraps in `overflow-x-auto`; adding a second page-level wrapper would be inert. | ui.tsx:160-165 read verbatim. | M3 kept the wrapper and only changed the phone branch. |
| `/v1/logs` filter fields are ALL bounded at parse time (`EventFilter`, event_log.rs:305-333); a client that omits `limit` or sends an unbounded `routes`/`text` list is a DoS-shaped caller. | event_log.rs:305-333 read verbatim. | M4 uses an explicit bounded `limit`. |
| AGENTS.md sits at 30,719 bytes against its ~30,720 cap; no new prose can be appended without a full compaction pass. | fs_stat 2026-11-09. | Filed as PRD row `agents-md-size-cap-blocks-prose-addition` instead of an inline doc edit. |
