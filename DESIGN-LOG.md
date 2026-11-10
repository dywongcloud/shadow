# DESIGN-LOG.md

Mode: **Adaptive** (MAYA, Most Advanced Yet Acceptable). Reason: the brief serves a public
audience on real phones and names approachability explicitly ("not responsive", "get clunky"),
so both the advanced pole and the acceptable pole must hold. Advanced = the selected-database
metrics/logs view actually becomes legible and honest at 320px; Acceptable = no existing crumb,
link, action or measured value is lost. Anchors for both poles: `maya` and `krug`.

## Step 0. Tooling inventory

| Job | Tool used | Fallback |
| --- | --- | --- |
| Code/symbol search and call sites | gm `codesearch` / `callers` / `impact` (one `gm_codemode` dispatch) | none needed |
| Anchor graph render/navigate | none available (no Mermaid renderer in this harness) | exact Mermaid node ids + edge labels recorded below, from `references/anchor-graph.md` read verbatim |
| Frontier computation | none available (no graph-query tool) | edge lines of the graph read by hand |
| Workflow/pool state | gm `pool_observe`, `prd-add`, `phase-status` | `.gm/pool/*.live` heartbeats |
| Panel | Claude Code `Agent` tool, ONE agent per literary reference, 17 lanes | n/a |
| Persist log | gm `fs_write` (project-only) | n/a |
| Render | gm `browser` headful CDP requested in lane sub08 | UNRENDERED if no dev server runs; declared below |

## Step 1. Traversal Plan

Brief in one sentence: make the shadw wordmark, the /storage list and the metric views genuinely
responsive on phones without losing a single existing affordance, then enrich the selected
database under /storage with a measured metrics and logs view.

Seed (exact labels, copied verbatim from the anchor graph):
- Stance `mauM` "An Incomplete Manifesto for Growth (Bruce Mau)"
- Rule `mb` "Grid Systems in Graphic Design (Josef Müller-Brockmann)"
- Rule `vh` "Visual Hierarchy (Arnheim, Dondis)"
- Rule `wcag` "WCAG 2.2 Contrast and Perceivability (W3C)"
- Breaker `samBreak` "Making and Breaking the Grid (Timothy Samara)"
- Counterpoint `maya` "MAYA: Most Advanced Yet Acceptable (Raymond Loewy)", reached by the
  dotted edge `samBreak -.->|loosens| mb` and `maya -->|bounds the break of| samBreak`.

Chain: "An Incomplete Manifesto for Growth (Bruce Mau)" --process maxims for--> "Double Diamond"
--explore options with--> "Morphological Box" --select boldest with--> "Pugh-Matrix" --test with-->
"Spike Solution"; and "Grid Systems in Graphic Design (Josef Müller-Brockmann)" --codifies-->
"International Typographic Style (Swiss Style)" <-loosens- (dotted) "Making and Breaking the Grid
(Timothy Samara)".

Initial Frontier (one hop from the seed, both directions):

| Candidate | Reached via (edge label, direction) | From anchor | Status |
| --- | --- | --- | --- |
| M1 shrink-proof brand + compact crumb | `vh -->|emphasis and subordination|` (outgoing) | Visual Hierarchy | TAKEN (sub01) |
| M2 responsive Stat/PageHeader primitives | `mb -->|codifies|` (incoming grounds) | Grid Systems | TAKEN (sub02) |
| M3 storage card fallback under sm | `samBreak -.->|loosens| mb` (dotted counterpoint) | Making and Breaking the Grid | TAKEN (sub03) |
| M4 group all measured gauges + bounded logs | `maya -->|dual criterion for| c_adr` (outgoing) | MAYA | TAKEN (sub07) |
| M5 one-column metric floors | `wcag -.->|accessibility floor under| carson` (dotted) | WCAG 2.2 | TAKEN (sub06) |
| Premortem objections | `c_premortem -->|anticipated failure recorded in| c_adr` (incoming) | Premortem | OPEN (R0 below) |
| Deletion review of inherited crumbs | `c_chest -->|know why the rule exists| samBreak` (incoming) | Chesterton's Fence | OPEN |
| Final-state re-measurement | `c_keep -->|changelog of moves for| c_adr` (incoming) | Keep a Changelog | OPEN |

## Round R0. Premortem, WHOLE panel on the inherited artifact

Artifact examined: the ORIGINAL files as read before any edit (unrendered; no dev server).
Measured inherited state, all by gm `fs_read`/`codesearch` with exhaustive:true:
- `ui/components/topnav.tsx:101-163`: one `h-[52px]` flex row; `ui/components/logo.tsx:4-11`
  renders two `next/image` wordmarks (intrinsic 274x91) inside an `inline-flex` span with no
  `shrink-0`; the brand `Link` has no `shrink-0`; with a project selected the row holds
  brand + Slash + team switcher + Slash + project + optional Workflows crumb + 4 right controls.
- `ui/app/storage/page.tsx:130-164`: 8-column `<Table>`; `ui/components/ui.tsx:160-165`
  wraps it in `overflow-x-auto` (load-bearing, keeps the PAGE from scrolling).
- `ui/app/storage/[id]/instance-metrics.tsx:112-118`: `grid grid-cols-2 gap-3 sm:grid-cols-4`
  with `entries.slice(0, 8)` - measured gauges silently dropped.
- `ui/components/ui.tsx:13-31`: `Stat` value is `text-3xl`, no `min-w-0`.

Statements of meaning: the inherited UI says "this is a desktop console that happens to also
load on a phone" - the wordmark compresses, the table scrolls sideways, numbers clip.

Artist questions: is the 274:91 wordmark meant to be a fixed mark or a fluid one? Is the 8-column
table the canonical representation, or just the widest one?

Critic questions (neutral): which of the 8 columns are actually needed at 320px? Does a hidden
crumb remove a navigation affordance a one-handed user needs? Is "not measured" ever shown as 0?

Opinions (permitted after the questions): the inherited design earns its table (dense comparison
is a real job) but does not earn a 2-column floor at 320px nor a silent 8-gauge cap.

Andon pulled: yes, provisionally, by Critic: Inclusion (Mace) - a 2-column grid with `text-3xl`
numerals under 200% zoom is an access failure, not a preference. Resolution is M2 + M5.

Frontier candidates opened by R0: every objection above is attached to the move that answers it.
## Decision Records

### Move M1: shrink-proof brand, compact crumb (KEEP-THE-RULE)
Depends on: none
Anchors: Stance "An Incomplete Manifesto for Growth (Bruce Mau)" | Rule "Visual Hierarchy (Arnheim, Dondis)" | Rule "Grid Systems in Graphic Design (Josef Müller-Brockmann)" | Breaker "Making and Breaking the Grid (Timothy Samara)" | Counterpoint "MAYA: Most Advanced Yet Acceptable (Raymond Loewy)" (dotted edge `samBreak -.->|loosens| mb`)
Intent: at 320px with a project selected, the 274:91 wordmark keeps its proportion and the eye reads brand > project, without a second row of chrome.
Formal argument: claim - an `inline-flex` span with no `shrink-0` inside a `min-w-0` flex row is compressed by the crumb chain; grounds - topnav.tsx:101-141 and logo.tsx:4-11 read before the move; warrant - Visual Hierarchy's emphasis/subordination and the Grid Systems rule that a fixed module never absorbs another module's overflow.
Alternatives rejected: (1) a second stacked header row - costs 52px of vertical space and moves the tabs below the fold; (2) `display:none` on the wordmark below sm - loses the brand and its link, violating the acceptable pole.
Consequence: hiding non-essential crumbs below sm means the team switcher may need an `aria-label` on the compact control; the Workflows crumb disappears on phones and must remain reachable through the section tabs.
Fence: no rule broken.
Frontier effect: opens "does the hidden crumb keep its accessible name" (Inclusion) and "is the section tab bar still scrollable" (Usability).
Mode field: Advanced pole - a genuinely aspect-correct brand under a 4-crumb 320px header, anchored by `vh`; Acceptable pole - every crumb link and the sticky 52px header survive, anchored by `krug`.

### Move M2: responsive Stat / PageHeader primitives (KEEP-THE-RULE)
Depends on: none
Anchors: Rule "Grid Systems in Graphic Design (Josef Müller-Brockmann)" | Rule "WCAG 2.2 Contrast and Perceivability (W3C)" | Counterpoint "MAYA"
Intent: a measured number never clips at 320px or at 200% zoom, in ANY view that uses the shared primitive.
Formal argument: claim - `text-3xl` inside `p-4` Card and a 2-column grid cannot fit a 9-digit tabular number at 320px; grounds - ui.tsx:13-31 read before the move; warrant - WCAG 1.4.4 resize text and Müller-Brockmann's grid as a field of proportions, not fixed sizes.
Alternatives rejected: (1) per-page overrides - fixes one symptom and leaves the class of failures; (2) `overflow-hidden` on the value - hides measured data, which this project forbids.
Consequence: every caller of Stat gets smaller type at phone widths; any caller with a hard-coded height must be checked (enumerated exhaustively by sub02).
Fence: no rule broken; the public prop signature is deliberately unchanged.
Frontier effect: opens "does a caller className override the responsive type at equal specificity" (CSS cascade verifier).
Mode field: Advanced - one primitive fixes the whole class; Acceptable - no prop signature changes, no caller breaks (`krug`).

### Move M3: storage card fallback under sm (BREAK)
Depends on: M2
Anchors: Breaker "Making and Breaking the Grid (Timothy Samara)" | Rule "Grid Systems in Graphic Design" | Rule "Visual Hierarchy" | Counterpoint "MAYA"
Intent: the eye sees eight databases as eight legible objects on a phone, not one table sliced by a horizontal scroll.
Formal argument: claim - a horizontally scrolling 8-column table is a desktop grid transported unchanged; grounds - storage/page.tsx:130-164; warrant - Samara's Making and Breaking the Grid: break the grid when the content's proportions change, and `vh`'s emphasis: a card gives the name the weight a row cannot.
Alternatives rejected: (1) keep the table and add `overflow-x-auto` (already present, ui.tsx:160-165) - that IS the clunky inherited state; (2) hide 5 of the 8 columns below sm - loses measured fields and violates the acceptable pole.
Consequence: two render branches must stay field-identical forever; the BrowseStorage sheet gets a second layout. Removing the sheet's fixed width is a deletion of a competing system.
Fence: the rule being broken is the single `Table` representation at all widths. It exists so dense desktop comparison stays possible; the Table is therefore KEPT at sm+ and only the phone branch breaks from it.
Frontier effect: opens deletion review (2f), field-parity verification, and the "smoother spectacle" objection from Debord.
Mode field: Advanced - a card grammar that actually uses the phone's vertical axis (`samBreak`); Acceptable - same hrefs, same fields, same actions (`maya`, `krug`).

### Move M4: all measured gauges + bounded database logs (BREAK)
Depends on: M2, M3
Anchors: Stance "An Incomplete Manifesto for Growth" | Breaker "Making and Breaking the Grid" | Counterpoint "MAYA" | Rule "Visual Hierarchy"
Intent: a selected database tells the whole truth about itself - every measured gauge, its freshness, its gateway counters, and its own recent events - on a phone.
Formal argument: claim - `entries.slice(0, 8)` silently discards measured data and the view has no event history; grounds - instance-metrics.tsx:100-145 read before the move; warrant - Mau's "grow, don't decorate" and Tufte's graphical integrity: a number with no provenance or a hidden measurement is a lie.
Alternatives rejected: (1) a new backend endpoint for database logs - no such route exists and inventing one is forbidden; (2) an unbounded `/v1/logs` poll - the backend bounds every filter at parse time, so this would be a DoS-shaped client.
Consequence: a second poller on the detail page; a tab switch adds a state the existing console view did not have. Every new wire field must be OPTIONAL with an exact fallback.
Fence: the rule broken is "one card of eight gauges". It existed to bound visual density, so the replacement must GROUP gauges and state freshness rather than simply lengthening the wall of numbers.
Frontier effect: opens honesty verification (Tufte/Cairo), tenant-scope verification of /v1/logs, and the role-matrix check.
Mode field: Advanced - grouped small multiples with real freshness and scoped events (`maya`); Acceptable - default tab stays the existing view, credentials/console/rotate unchanged (`krug`).

### Move M5: one-column metric floors across usage/monitoring/observability/logs (KEEP-THE-RULE)
Depends on: M2
Anchors: Rule "WCAG 2.2 Contrast and Perceivability (W3C)" | Rule "Grid Systems in Graphic Design" | Counterpoint "MAYA"
Intent: no metric view anywhere forces a horizontal PAGE scroll at 320px.
Formal argument: claim - a `grid-cols-2` floor with `text-3xl` numerals cannot hold at 320px; grounds - R0 measurements; warrant - WCAG 1.4.10 reflow (no two-dimensional scrolling for content at 320 CSS px).
Alternatives rejected: (1) `zoom` on the whole page - breaks tap targets; (2) `overflow-x-auto` on the page - explicitly two-dimensional scrolling.
Consequence: wider-element wrappers only; the wide element scrolls, the page does not. More vertical length on phones.
Fence: no rule broken.
Frontier effect: opens the final-state re-scan (sub04-r1) and the 200%-zoom objection (Mace).
Mode field: Advanced - reflow that works at 200% zoom; Acceptable - every existing poller, endpoint and measured value preserved.
## Step 2f. Deletion review and residual diff

Substantial elements that predate this run, and what each earns:

| Inherited element | What it earns | Decision |
| --- | --- | --- |
| 8-column `<Table>` on /storage | dense desktop comparison IS a real job; nothing on a phone replaces it | KEEP at sm+, BREAK below sm (M3) |
| `overflow-x-auto` wrapper in `ui.tsx:160-165` | keeps the PAGE from scrolling horizontally on desktop narrow windows | KEEP - removing it would create an unbounded page scroll |
| `h-[52px]` sticky header + safe-area padding | one predictable chrome height across every route; the section tab bar is already scrollable | KEEP - a second row costs more than it buys |
| brand + Slash + team switcher crumb chain | orientation: which team, which project, which deployment | KEEP as links; HIDE only the non-essential crumbs below sm, never delete them from the tree |
| `entries.slice(0, 8)` gauge cap | bounded visual density on a card | SCRAP - replaced by grouping + freshness (M4); a measured gauge silently dropped is dishonest |
| `text-3xl` Stat numeral | desktop scannability | ADAPT - responsive type floor (M2) |
| 2-column metric grid floor | compact desktop grids | ADAPT - one-column floor below sm (M5) |
| `p-6` / fixed-width BrowseStorage sheet | desktop breathing room | ADAPT - `p-4 sm:p-6`, full-width bottom sheet on phones (M3) |
| `measured` / `not measured` badge and `data.note` | the honest absence that separates "the node did not measure this" from zero | KEEP - load-bearing (AGENTS.md: a value the node did not measure renders as absent) |
| `/v1/logs?limit=200` poller and its bounded EventFilter | bounded, tenant-scoped event history; no new endpoint needed | KEEP - reused for the database-scoped view (M4) |
| `SqliteDatabaseDetail` branch, credentials/reveal/rotate/destroy | tenant-facing destructive and secret paths | KEEP - untouched (M4) |

Retained-value ledger: every deletion above must preserve (a) the exact row href, (b) every
measured field, (c) the accessible name of every control, (d) the default view a returning user
sees, and (e) the distinction between absent and zero. sub03 and sub07 verify (a)-(d) by field
enumeration; sub14/sub16 verify (e).

Residual diff (computed values that change, classified):
- Brand rendered width at 320px: intended change (aspect preserved instead of compressed).
- Number of /storage columns visible at 320px: intended (8 stacked fields, 0 horizontal page scroll).
- Number of gauges rendered for a selected database: intended (all measured, grouped) - regression
  risk is a longer wall of numbers, mitigated by grouping and the Cairo/Tufte panel.
- Desktop `sm+` render: intended INERT - verified by the `hidden sm:block` branch and by
  sub02's exhaustive caller enumeration.
- Poller count on the detail page: intended (+1, bounded `/v1/logs`); regression risk is an
  unbounded filter, checked by sub05/sub07.
- Any figure printed by the UI: measured from the node or absent; no new constant is introduced.
## Anchor Ledger

| Anchor | Role | Status | Evidence | Replacement or note |
| --- | --- | --- | --- | --- |
| "An Incomplete Manifesto for Growth (Bruce Mau)" | Stance | KEEP | M1/M4 grew the work (all gauges, all crumbs survive) rather than decorating it; strongest objection survived: Debord's "smoother spectacle" | none |
| "Grid Systems in Graphic Design (Josef Müller-Brockmann)" | Rule | ADAPT | M2/M3/M5: the grid's proportions become responsive; the `sm+` table stays a grid | `mb` retained at desktop, `samBreak` governs the phone branch |
| "Visual Hierarchy (Arnheim, Dondis)" | Rule | KEEP | M1 brand > project > controls; strongest objection survived: Arnheim's header balance at 320px | none |
| "WCAG 2.2 Contrast and Perceivability (W3C)" | Rule | KEEP | M5 one-column floor satisfies reflow; strongest objection survived: Mace at 200% zoom | none |
| "CRAP Principles: Contrast, Repetition, Alignment, Proximity (Robin Williams)" | Rule | KEEP | M3 card list groups by proximity; inherited | none |
| "Making and Breaking the Grid (Timothy Samara)" | Breaker | ADAPT | M3 BREAK taken; the fence (dense desktop comparison) is preserved by keeping `sm+` Table | declined ORNAMENT version (a second stacked header row) |
| "MAYA: Most Advanced Yet Acceptable (Raymond Loewy)" | Counterpoint (dotted from `samBreak`) | KEEP | governing criterion for the whole Adaptive run; both poles recorded per move | none |
| "Premortem" (`c_premortem`) | Catalog | KEEP | R0 above | none |
| "Chesterton's Fence" (`c_chest`) | Catalog | KEEP | 2f deletion review asks why every inherited element existed | none |
| "Keep a Changelog" (`c_keep`) | Catalog | KEEP | final CHANGELOG.md entry | none |
| "The Society of the Spectacle (Guy Debord)" | Lens / Critic: Provocateur | pending panel | - | dotted edge to Critic: Usability (Krug) |
| "Art as Technique (Viktor Shklovsky)" | Lens / Critic: Provocateur | pending panel | - | - |
| "Mismatch: How Inclusion Shapes Design (Kat Holmes)" | Lens / Critic: Inclusion | pending panel | - | dotted edge to Critic: Cross-Cultural (Escobar) |
| "The Principles of Universal Design (Ronald Mace)" | Lens / Critic: Inclusion | pending panel | - | - |
| "Don't Make Me Think (Steve Krug)" | Lens / Critic: Usability | pending panel | - | dotted edge to Debord |
| "The Visual Display of Quantitative Information (Edward Tufte)" | Lens / Critic: Evidence | pending panel | - | - |
| "How Charts Lie (Alberto Cairo)" | Lens / Critic: Evidence | pending panel | - | dotted edge to Critic: Wit (Fletcher) |
| "Art and Visual Perception (Rudolf Arnheim)" | Lens / Critic: Formalist | pending panel | - | dotted edge to Critic: Perception (Kahneman) |
| "The Wisdom of Crowds (James Surowiecki)" | Panel method | KEEP | 9 independent critic agents, one per literary reference, no composite voice | none |
| "Critical Response Process (Liz Lerman, John Borstel)" | Panel method | KEEP | meaning -> artist questions -> neutral questions -> opinions, in that order | none |

Live graph (no Mermaid renderer available; exact ids and edge labels recorded here):
`class mauM,mb,vh,wcag,samBreak,maya,c_premortem,c_chest,c_keep,crit_prov_debord,crit_prov_shklovsky,crit_inc_holmes,crit_inc_mace,crit_use_krug,crit_evid_tufte,crit_evid_cairo,crit_formal_arnheim,surowiecki,lerman visited`
`class mb,vh,samBreak adapted`; `class mauM,wcag,crap,maya,c_premortem,c_chest,c_keep,surowiecki,lerman kept`.
Dotted edges leaving anchors used, each applied or declined:
- `samBreak -.->|loosens| mb` - APPLIED (M3 breaks the phone grid but keeps `sm+`).
- `maya -.->|tempers strangeness of| shklovsky` - DECLINED with reason: this is a production console, not an art object; making the row strange would break the acceptable pole.
- `maya -.->|acceptability limit on| carson` - DECLINED: no expressive typography experiment belongs in a billing-adjacent dashboard.
- `wcag -.->|accessibility floor under| carson` - N/A: carson is not on the active path.
- `c_chest -.->|gate passed before| weingart` - DECLINED: weingart's Basel typographic break is outside this medium's active path.
- `crit_prov_debord -.->|attacks smoothness favored by| crit_use_krug` - APPLIED as the panel's required dotted pair.
- `crit_inc_holmes -.->|questioned by| crit_plur_escobar` - DECLINED with reason: Escobar's universalism critique is not instantiated in this panel; recorded so S5 is honest rather than claimed.
- `crit_evid_cairo -.->|demands honesty from| crit_wit_fletcher` - DECLINED: no Wit seat was convened in this Adaptive panel; the honesty demand is enforced directly by the Cairo agent.
### Panel, Move M1 / WHOLE, round R1  (artifact examined: INHERITED tree, unrendered)
| Critic | Anchor used | Observed in the work | Verdict | Requested change |
| --- | --- | --- | --- | --- |
| Critic: Formalist (Arnheim) | "Art and Visual Perception" | `logo.tsx:6` span and `topnav.tsx:103` Link have NO `shrink-0`; `topnav.tsx:102` crumb chain is `min-w-0`+truncate; `ui.tsx:32` keeps `text-3xl` while `ui.tsx:152` title is `text-xl sm:text-2xl`; `storage/page.tsx:130-164` 8-column table; sheet's two provider groups share one `Row` shape at `:508` | OBJECT | add `shrink-0` to the brand span and Link so 274:91 keeps its vector; let the crumb chain absorb the deficit |

Statements of meaning: the header says "actions outrank orientation" - four right-side controls are the only well-grouped mass while the brand is the first element surrendered.
Artist questions: is `mb-6` at storage/page.tsx:364 meant to carry the boundary between the two provider groups, or is that the section labels' job?
Critic questions: neutral - see the artist question above.
Opinions (last): unbalanced field, empty center, deformed brand vector, one similarity-group instead of two proximity-groups.
Andon pulled: YES, by Critic: Formalist (Arnheim) - a deformed brand vector at 320px.

**Premise test (dada Step 2d).** The critic's own dispatch `1794288138392-7088-78fd03822f889e3d` read `git_status` showing ONLY untracked `DESIGN-LOG.md`: all four source surfaces were unmodified. The objection is therefore TRUE of the INHERITED artifact and FALSE as a description of the final one - it is the R0 premortem objection restated. Resolution: **ADAPT**, not SCRAP: the requested change is identically M1 (already dispatched to sub01 at `a1ffac295af2db400`). The critic MUST be re-run on the final tree; its verdict is not carried as a final-state objection. Dependent moves M3/M4/M5 are marked **REOPENED** until that re-run returns.

Recorded: `1794288222699-7204-dbdcb991605a2746`, corroborating exhaustive scan `1794288542893-7511-411d5ced1734aaf3`.
### Panel, Move M4 / M5, round R2  (artifact examined: INHERITED tree, unrendered)
| Critic | Anchor used | Observed in the work | Verdict | Requested change |
| --- | --- | --- | --- | --- |
| Critic: Evidence (Tufte) | "The Visual Display of Quantitative Information" | `instance-metrics.tsx:49-65` carries units but `:93-97` has NO sampled-at time; `ui/lib/api.ts:1208` declares `observed_at_ms?` with exactly ONE occurrence in 644 files / 264,883 lines (the declaration); `usage-view.tsx:57-62` collapses five missing metrics to `?? 0`, `:71` invents `includedTotal: 20`, `:187-190` draws that invented $20 as a proportional bar, `:21-23` builds a sparkline of N identical points from one scalar, `:227` removes a 3-colour chart's legend; `instance-metrics.tsx:113-116` silently caps at 8 gauges | OBJECT | render `data.observed_at_ms` as an absolute "sampled HH:MM:SS  -  every 20 s" label beside the `measured` badge |

Statements of meaning: a reader can tell a gauge was NOT measured but cannot tell WHEN a measured one was taken; absence renders as "-" in one section and as $0.00/$20.00 in another.
Artist questions: does `/v1/databases/<id>/metrics` populate `observed_at_ms` on every measured response, or only some nodes?
Critic questions: neutral - see above.
Opinions (last): the honest absence badge is real but spent on decoration while the observation time is missing.
Andon pulled: YES, by Critic: Evidence (Tufte).

**Premise test (2d).** The freshness/8-cap part is TRUE of the inherited tree and is answered by M4 (already dispatched to sub07). The `?? 0` / invented $20 / identical-point sparkline part was NOT in the original brief and is a NEW finding the critic surfaced - it is TRUE and violates the load-bearing "Dashboard numbers are measured or absent, never invented" rule. Resolution: **ADAPT** - routed verbatim to the owning writer sub06 via `SendMessage` at this round, and the freshness label to sub07. Both rows stay OPEN until the final-state re-measure confirms the numbers are gone. This is the finding that changes the artifact most, so it takes priority over the remaining Frontier candidates.

Recorded: primary witness `1794288684666-7644-19aadaa077c2f2e5` (exhaustive `observed_at_ms` scan), supporting `1794288568792-7537-9f32e00cf2ddf2da`, `1794288490054-7457-9fb81b1f054d454f`, `1794288740056-5147-ef391cf743761263`.
### Panel, WHOLE, round R3 - R0/R1 objections resolved on the LANDED tree
Artifact examined: working tree after 9 source files landed (unrendered; no dev server).

| Critic | Anchor used | Observed in the work | Verdict | Requested change |
| --- | --- | --- | --- | --- |
| Critic: Formalist (Arnheim), R1 | "Art and Visual Perception" | BEFORE: `logo.tsx:6` span `inline-flex items-center` (no `shrink-0`), `topnav.tsx:103` Link `flex items-center`. AFTER: `logo.tsx:6` is `inline-flex shrink-0 items-center`; both Images carry `max-w-none`; `topnav.tsx:103` Link is `flex shrink-0 items-center`; `topnav.tsx:102` crumb group is `flex min-w-0 flex-1`; `topnav.tsx:145` right controls are `flex shrink-0` | **PENDING** - the class-string change is measured, but the objecting critic's final-state re-judge is still running; no PASS is claimed until it returns | none |
| Critic: Inclusion (Mace), R0 | "The Principles of Universal Design" | `Slash` is now `hidden ... sm:block` by default with an explicit `mobile` opt-in (`topnav.tsx:77-78`); the project crumb uses `<Slash mobile />` at :110 so no dangling separator; `tap-44` kept on docs/auth Links; deployment badge hidden below sm at :128 | **PASS** | re-check when `Stat` lands |
| Critic: Usability (Krug), cross-check | "Don't Make Me Think" | all 12 `<Logo` sites enumerated exhaustively (`1794289485465-5733-b9d034dd3177ba99`, 644 files / 264,945 lines, exhaustive:true); the 4 shared-component + 3 marketing + 2 auth + footer + docs sites all carry `w-auto` + `max-w-none` + a shrink-proof Link; the 2 integrations hits are a DIFFERENT local `Logo`, and the vendor hit is out of scope | **PASS** | none |

Measured deltas (computed from the source, not from the patch having landed):
- shared-brand `shrink-0` sites: 0 -> 6 (logo.tsx span, topnav.tsx Link, auth-shell.tsx x2, footer.tsx, docs/layout.tsx).
- wordmark sites with `w-auto max-w-none`: 0 -> 6 in the shared component's callers; marketing-shell.tsx's own `Logo` gained `max-w-none shrink-0` and `h-6 sm:h-8`.
- /storage phone columns: 8 horizontally-scrolling -> 8 stacked `dt/dd` fields in `sm:hidden` cards; `sm+` Table unchanged (`hidden sm:block`).
- responsive `Slash`: 1 unconditional -> 5, with 1 `mobile` opt-in.

Andon status: R1 Andon still OPEN - the Arnheim final-state re-judge was dispatched to the same agent at this round and has NOT returned; the measured class-string deltas above are evidence of the change, NOT a critic verdict. R2 (Tufte) Andon also OPEN pending the `observed_at_ms` label and the invented-number removal. Neither is claimed cleared until its own witness returns.
### Panel, Move M4, round R4 - ROUTE-INVENTORY REFUTATION (the sharpest disagreement)
The orchestrator's own M4 brief asserted "scope /v1/logs with a route/text filter over /v1/databases + the database id" and "ingress/egress counters from the metering plane". The research lane (sub05) plus its adversarial successor (sub05-r1, 4 dispatches) REFUTED BOTH against the backend parse sites:

| Claim in the brief | Refutation (path:line) | Resolution |
| --- | --- | --- |
| `?routes=` / `?text=` on /v1/logs | `admin.rs:8906-8958` LogsQ has `route` (singular) and `q`; `routes`/`text` are INTERNAL `EventFilter` fields (`event_log.rs:321,327`). No `deny_unknown_fields`, so unknown params are SILENTLY IGNORED and the answer is UNFILTERED. | SCRAP the premise; corrected brief sent to sub07 |
| /v1/logs can be scoped to one database | No `db_id`/`database`/`database_id` param; `Event` (`state.rs:94-118`) has no database field; `db_rest.rs:88` writes gateway requests to `db_metrics`, not the Event log. | SCRAP; the logs section must be labelled "tenant-wide events mentioning this id" or omitted with a stated blocker |
| ingress/egress counters come from the metering plane | `usage_metering.rs:252` `require_operator` -> 403 for a tenant. `admin.rs:12688` admits the owning tenant to `/v1/databases/:id/metrics`, whose `counters` (`db_metrics.rs:505-527`) already carries `bytes_in`/`bytes_out`. | SCRAP; use the metrics-route counters. `/v1/usage/metering` MUST NOT appear in tenant dashboard code |
| a Rust `DbInstanceMetrics` struct exists | exhaustive `codesearch` literal over 723 `.rs` files = 0 hits; the name exists only in `ui/lib/api.ts:1197` | SCRAP (phrasing) |

This is a case where the panel was right to distrust the PLAN rather than only the artifact. The falsification changed the artifact: the logs view cannot claim to be database-scoped, and the byte counters come from a tenant-readable route. M4 is **REOPENED** with the corrected contract. Roles check: `GET /v1/logs` and `GET /v1/databases/:id/metrics` are viewer-by-default (unlisted GET reads), with handler-local tenant scoping - so no role gate needs changing.
### Panel, WHOLE, round R5 - final-state re-judges and the Inclusion objection
| Critic | Anchor used | Observed in the work | Verdict | Requested change |
| --- | --- | --- | --- | --- |
| Critic: Formalist (Arnheim), FINAL RE-JUDGE | "Art and Visual Perception" | re-read AFTER all four artifacts landed (dispatches `1794290558736-6665-407c3d94fcaafffe`, `1794290560111`, `1794290680559`). `logo.tsx:6-8` + `topnav.tsx:103` brand `shrink-0`/`max-w-none`/`w-auto`; `topnav.tsx:102` `min-w-0 flex-1` with truncate at :116/:127/:140; `topnav.tsx:145` `shrink-0` right cluster; computed at 320px: 288px content - ~152px controls = 136px left, brand ~60px fixed, ~68px crumb; `storage/page.tsx:130,135,280-281,432,451` card fallback + two contour-separated provider groups; `ui.tsx:32,154` both `text-xl` at 320px | **PASS - R1 objection RESOLVED on all four counts** | ONE: `ui.tsx:32` `text-xl` -> `text-lg` so the numeral stays subordinate to the title at 320px |
| Critic: Inclusion (Holmes) | "Mismatch" | `.tap-44` (globals.css:217-224, 44px `!important` coarse-pointer floor) occurs 44 times / 19 files but 0 times in the 5 new surfaces; `theme-toggle.tsx:16-18` has `aria-label` + `tap-44` in the SAME 52px row while `topnav.tsx:161` is `h-8 w-8` title-only; `storage/page.tsx:268` copy button is ~14px with a `title` only; hidden crumbs at `topnav.tsx:105,137,186`; `instance-metrics.tsx:93-96,111` DOES name its absence | **OBJECT** | add `tap-44` at `topnav.tsx:161` and `storage/page.tsx:268` |
| Critic: Evidence (Tufte), FINAL RE-JUDGE | "The Visual Display of Quantitative Information" | re-read at `1794290142045`: both files byte-identical to R2 at that moment; all six integrity defects STILL STANDING, including 11 `?? 0` and `: 20` | **OBJECT (at that timestamp)** | absolute sampled-at label; no invented credit |
| Critic: Evidence (Cairo) | "How Charts Lie" | `instance-metrics.tsx:128` fleet-summed gateway requests with NO scope label though `sources`/`served_by` ride the wire (`db_metrics.rs:533-538`); `usage-view.tsx:144,152` node-local `mesh_local()` slice presented as tenant total (`admin.rs:16502,16505-16520`); freshness not derived from measured poll time; `ui.tsx:17,30` `Stat` hint slot unused | **OBJECT (on inherited tree)** | ONE provenance/scope line beside each counter, from fields the payload ALREADY carries, using the Stat hint slot |

Tufte and Cairo both judged BEFORE the usage-view rewrite landed. Their objections are answered by the LANDED `measured()` primitive (verified below in Step 2g), but neither has re-judged that final state - recorded honestly.

**Andon status:** R1 CLEARED (Arnheim PASS). Holmes Andon OPEN (tap-44). Tufte/Cairo Andon OPEN pending a post-rewrite re-judge. Shklovsky Andon OPEN pending the two-tier card.
## Step 2g. Final-state re-measurement

Measured AFTER the last source edit, by the procedure stated beside each figure. Reproduce from
ui/ with the source-only tsc config (excludes .next), npm run lint, and gm codesearch with
mode literal/regex and exhaustive:true.

| Figure the work states | Procedure that reproduces it | Value at final state | Reproducible / corrected |
| --- | --- | --- | --- |
| "12 <Logo render sites" | gm codesearch regex scoped to ui, exhaustive | 12 hits / 8 files / 644 files scanned / 264,945 lines; 9 shared-component/auth/footer/docs/marketing sites shrink-proof, 2 integrations hits are a DIFFERENT local Logo, 1 vendor hit out of scope | reproducible, dispatch 1794289485465-5733-b9d034dd3177ba99 |
| "8 fields in the /storage card" | read storage/page.tsx card branch, enumerate dt elements | 8: Name, Provider, Type, Project, Region, Connection, Status, Created; Name/Status form a primary two-tier line and the 8 dt labels are sr-only so accessible names survive | reproducible, corrected from the flat dl by the Shklovsky ADAPT |
| "14 sections" | teamTabs.length in topnav.tsx:20 | 14, declared in a details element at :169-178 with all 14 hrefs enumerated below sm | reproducible |
| "every 20 s" | POLL_MS / 1000 in instance-metrics.tsx | 20 s, rendered beside observed_at_ms as an absolute UTC sampled time; a missing observed_at_ms reads "observation time not reported", never a fabricated now | reproducible |
| "100 max" events | LIMIT in database-events.tsx | 100, with /v1/logs?limit=100&route=/v1/databases&q=<dbId>; response sliced to 100; badge reads "N of 100 max" | reproducible |
| "11 ?? 0" | gm codesearch literal "?? 0" in usage-view.tsx, exhaustive | 0 remaining in compute/billing values; replaced by measured()/sumMeasured()/chargeMeasured() returning null -> "-". Per-bucket "?? null" chart values remain and are correct absences, not invented zeros | corrected |
| "included $20 credit" | gm codesearch literal ": 20" in usage-view.tsx | 0; includedCents/usedCents/balanceCents only, the bar is hidden when the ratio cannot be derived | corrected |
| "8 capped gauges" | gm codesearch literal slice(0, 8) under ui/app/storage | 0; all entries grouped into 4 named sections + Other, nothing dropped | corrected |
| "source errors = 0" | tsc --noEmit --project /private/tmp/hive-tsconfig-sourceonly.json | 0, exit 0 on the second run after the last edit. Full-project tsc --noEmit exits 2 with 4 diagnostics ALL in stale generated .next/*/validator.ts (blog, case-studies, /status routes absent from ui/app, exhaustive filename scan = 0); SOURCE_ERRORS=0 | reproducible; generated-artifact blocker filed on the row |
| "lint clean" | npm run lint from ui/ | exit 0, 4 lines (banner only), final run after all edits | reproducible |
| "no horizontal page scroll" | class-string audit: min-w-0 w-full max-w-full, one-column grid floors, element-level overflow-x-auto only; globals.css:214 clip scoped to html:has(#docs-content) | NOT browser-measured. The headful CDP witness is reported separately. Class-level evidence only, explicitly not a rendered measurement | UNRENDERED, honestly recorded |

The two enforced checks named in ui/AGENTS.md pass at final state on real source; next build was
never run. The generated-validator failures are pre-existing stale artifacts from a prior
next dev run with a different route set, not a regression; ui/next-env.d.ts was restored via
git_checkout (ui/AGENTS.md: never commit it).
## Step 3. Whole-artifact rounds

### WHOLE round W1 (R0..R5)
Rounds R0 (premortem), R1 (Arnheim), R2 (Tufte), R3 (landed resolution), R4 (route
refutation), R5 (Debord / Shklovsky / Holmes / Mace / Krug / Cairo / Arnheim re-judge).
Objections raised here about INTERACTIONS, not single moves:
- The route-inventory refutation (R4) is the sharpest: a plan-level claim that /v1/logs could
  be database-scoped was false; M4 was REOPENED with a corrected contract.
- Shklovsky (card as a stood-up table) x Mace (perceptible information) x Holmes (44px floor)
  interacted on the SAME card: resolving Shklovsky with visible-label deletion would have
  broken Inclusion. Resolved with `sr-only` dt labels, so both hold.
- Cairo (scope/provenance) x Tufte (freshness) x the metering-plane correction interacted on
  the selected-database view: all three are answered by one payload, counters with a derived
  scope line and an absolute sampled-at label.
New Frontier candidates from W1: declared-tab index (Debord), two-tier card (Shklovsky),
tap-44 on icon controls (Holmes), StateNotice (Mace), whole-row affordance (Krug),
absolute sampled-at label (Tufte), scope line (Cairo), text-lg hierarchy (Arnheim).

### WHOLE round W2 (final state, after every ADAPT landed)
| Candidate | Reached via | From anchor | Status |
| --- | --- | --- | --- |
| M1 shrink-proof brand | vh --> emphasis and subordination | Visual Hierarchy | TAKEN, witnessed: 3.0108 vs 3.0110 |
| M2 responsive primitives | mb --> codifies | Grid Systems | TAKEN, caller enumeration clean |
| M3 storage card + bottom sheet | samBreak -.-> loosens (dotted) | Making and Breaking the Grid | TAKEN, two-tier ADAPT + sr-only labels |
| M4 grouped gauges + bounded events | maya --> dual criterion for c_adr | MAYA | TAKEN with CORRECTED contract |
| M5 one-column floors | wcag -.-> accessibility floor | WCAG 2.2 | TAKEN across usage/observability/logs |
| Declared tab index | c_adr --> rupture recorded for samBreak | Debord Andon | TAKEN (14 enumerated, sm:hidden details) |
| tap-44 on icon controls | curbcut -.-> accessibility benefits everyone | Holmes Andon | TAKEN (topnav account Link, sheet copy button) |
| StateNotice announced errors | mace --> universal floor for wcag | Mace Andon | TAKEN (ui.tsx primitive, sheet footer, metrics/events errors) |
| Whole-row affordance | krug --> usability for nielsen | Krug Andon | ADAPTED honestly (see below) |
| Absolute sampled-at label | tufteV --> names c_tufte | Tufte Andon | TAKEN (observed_at_ms, UTC, every 20 s) |
| Counter scope line | cairoLie --> honesty with | Cairo Andon | TAKEN (counterScope from sources/served_by/node) |
| Measured-only numbers | c_qas --> testable scenario | Cairo/Tufte | TAKEN (measured() primitive, no $20, no flat sparklines) |
| text-lg hierarchy | vh --> scale decision | Arnheim final PASS | TAKEN |

W2 produced no new ADAPT, SCRAP, REOPEN or OPEN Frontier item. S3 saturation: W2 is the
second consecutive whole round with no new objection after W1 opened its final candidates;
no third round reopened any gate. **Honest qualification:** the Krug whole-row request was
ADAPTED rather than implemented literally - wrapping a `<tr>` content in a `<Link>` is invalid
HTML, and the card contains a copy `<button>` that cannot be nested in an anchor. The objection's
PREMISE (a false clickable affordance) is resolved by removing `cursor-pointer` from the tr,
adding a visible `ChevronRight` to the real name Link, and giving both Links `tap-44`. This is
recorded as an ADAPT with the reason, not claimed as the literal requested change.

## Step 4. Stop test

- **S1 Frontier no OPEN items:** MET - every candidate in W2 is TAKEN or ADAPTED with a reason.
- **S2 WHOLE round PASS from every required critic, no Andon:** **NOT MET.** Arnheim returned
  an explicit final-state PASS. The other seven seats returned OBJECT on the inherited or
  mid-land tree and their ADAPTs are witnessed in the tree, but they did NOT issue a final PASS.
- **S3 two consecutive WHOLE rounds with no new finding:** PARTIAL - W2 produced none, but W1
  produced 8, so two consecutive quiet rounds are not established.
- **S4 ambition push with a measured delta:** MET. M3 (the Breaker) was pushed further by the
  Shklovsky ADAPT from a flat 8-pair dl to a two-tier composition. Measured delta: brand
  ratio 0.2415 -> 3.0108 at 320px (headful CDP dispatch 1794291297227-7274-698e483b5bf63734);
  the intended `max-w-none` lever was measured INERT in isolation (the control already read
  3.0108 with maxWidth:100%) - `shrink-0` was the decisive lever. Both are logged.
- **S5 every dotted edge applied or declined:** MET - enumerated in the Anchor Ledger above,
  including two DECLINED with reasons and one N/A.

**Conclusion: the dada run is INCOMPLETE at S2/S3.** Open frontier: a final-state PASS from
Debord, Shklovsky, Holmes, Mace, Krug, Tufte and Cairo on the post-ADAPT tree. Resume point:
DESIGN-LOG.md "WHOLE round W2"; re-convene those seven seats read-only on the committed tree.
The coding request itself is complete and witnessed; this qualification concerns only the
design-workflow stop conditions, and is stated plainly rather than claimed as met.

## Double loop

The PANEL failed the work first: seven of eight critics judged the INHERITED tree because the
writers landed later than the panel was convened, so two rounds were spent re-judging a state
that no longer existed. The CRITERION then failed it: Adaptive/MAYA correctly forced the
Acceptable pole (every field and href survives), but the first M4 draft assumed a database-scoped
log route that the GRAPH of the code does not contain - a plan-level claim that only an
adversarial route-inventory lane refuted. Concrete amendment under "Graph amendments": add a
node `wire_contract_inventory` with a solid edge to `c_premortem` and a dotted edge to
`c_adr`, so every move that reads a backend field must cite an exhaustive route inventory
BEFORE its Decision Record is written. Process amendment: convene the premortem panel AFTER
the first writer wave lands, not before, or record every early verdict as R0-only by default.

## Compliance Check

- [x] Tooling inventory done, tool named per job - "Step 0. Tooling inventory" (gm Agent tool, gm fs_write/codesearch, host Read/Bash for checks; no Mermaid renderer available).
- [x] Mode stated with reason - "Adaptive" at the top, public audience + approachability named in the brief.
- [x] Seed with exact labels including one dotted-edge counterpoint, living Frontier - "Step 1. Traversal Plan" and W2 Frontier.
- [x] At least one BREAK taken (M3), no move quota used as a stop - the run ends on the S2/S3 qualification, not on a count.
- [x] Decision Record before each move with all fields - M1..M5, "Decision Records".
- [x] Panel Report per round, composition meets Adaptive (Provocateur Debord+Shklovsky, Inclusion Holmes+Mace, plus Krug, Tufte, Cairo, Arnheim = 8 seats, one agent per reference, dotted pair Debord/Krug and Holmes/Escobar lineage recorded) - R0..R5.
- [x] Every OBJECT resolved ADAPT/SCRAP/OVERRULE with dependents reopened - R1 ADAPT, R2 ADAPT, R4 SCRAP of the premise, R5 ADAPTs; M3/M4/M5 REOPENED where noted.
- [x] WHOLE rounds run; S1..S5 shown with where - "Step 3" and "Step 4"; S2/S3 explicitly NOT MET.
- [x] Every figure re-measured at final state with its procedure - "Step 2g".
- [x] Anchor Ledger complete, live graph updated - "Anchor Ledger" with exact node ids and class strings.
- [x] Double-loop paragraph written - above, with Graph amendments.
- [x] Final reply states mode, tools, stop conditions and everything skipped - in the terminal reply.

Skipped: a rendered screenshot of the full final composition (the headful CDP witness measured
computed values but no PNG was captured); a third WHOLE panel round; `next build` (forbidden by
ui/AGENTS.md); a final-state PASS from seven critics (S2); two consecutive quiet rounds (S3).
No Pugh-Matrix branch was run: no two Breaker candidates were simultaneously plausible, so the
branch condition in the workflow did not fire.
