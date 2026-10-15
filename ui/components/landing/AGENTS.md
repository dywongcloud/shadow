# ui/components/landing/AGENTS.md

The signed-out landing (`components/landing.tsx` composes the sections here) and the marketing shell around it: present-tense
conventions, what must be true of the code now; history lives in `CHANGELOG.md`. Loaded through `CLAUDE.md`; `ui/AGENTS.md` and
the root `AGENTS.md` still apply.

- **The copy sells outcomes, not architecture**: Git to a live URL, a preview per pull request, free to start, hosted or on your own
  machines. Peer-to-peer, mesh, QUIC and post-quantum belong to the docs, never to a hero, title, pill, card title, description or
  social card (the person's rule: "people dont care about peer to peer"). The globe is art, and its caption says so.
- **Every sentence is one the code backs.** An illustration is marked (`Illustrative` on the dome, terminal, preview card and
  keystone; `Sample` on every vignette that shows states, as `SampleTag`, the FIRST ROW of the vignette in the flow, so it can
  never lie on the content; one footnote per section) and names states, never counts, durations or prices.
  `public/product/dashboard-*.png` are real 2x captures of a seeded local node: recapture them when the dashboard changes, never
  retouch a frame.
- **Light is gradients, masks and stroke stacks, never a blur filter**; the sticky nav's backdrop blur is the page's only
  filter once the intro overlay (`intro-loader.tsx`: a blurred ring, 2.2 s, removed from the DOM) has ended; the headline paints at
  about 2.4 s behind it (60 ms with reduced motion), so never quote LCP as the page's speed (Chrome's LCP is the film-grain layer). The dome's limb (`LIMB` in `dome-light.tsx`) is traced from `public/globe-wireframe.png` (a new globe image needs a new
  trace), and nothing inside the dome glows except the tracers and nodes. The section washes are ambient light in the margins:
  text keeps 4.5:1 against the PIXELS behind it (a DOM walk cannot see a gradient).
- **Sizes are px.** The dashboard root is `font-size: 83.125%`, so every rem is smaller (`px-6` is 19.95 px, `text-sm` 11.6 px);
  `Section` is `max-w-[1200px]` + `px-6`, a 1160.1 px column, and the hero stage matches it.
- `landing.css` loads after Tailwind, so a class there that sets `position`, `margin` or `padding` beats the utility on the same
  element (`.lp-limb-top` sets no `position` for this reason): assert the COMPUTED value of anything that mixes the two.
- A flag, id, URL or algorithm name that can wrap is a `Tok` (or a `whitespace-nowrap` terminal segment); the code panel keeps a
  token of up to 30 characters whole and breaks a longer URL after its path slashes, never clipping it. Inline content in a flex
  row goes in ONE element, because a fragment becomes one flex item per run.
- **A list of capabilities is audited item by item**: `BUILD_TOOLCHAINS` names `go` and `python3` (the build image carries them), yet a repository whose
  root is a Go or Python project is an "unsupported root marker" (`app_discovery.rs`), so the toolchain line names npm, pnpm, yarn and bun and the
  Dockerfile lane, never Go or Python.
- A public page declares its metadata with `pageMeta` (`lib/page-meta.ts`): a page's own `openGraph` or `twitter` object REPLACES the root's, images
  included, so the helper names the share card on every page, and `title` is the page's own name (the root template adds " · shadw").
- Motion has one state, `html.lp-paused` (`motion-pause.tsx`): `PauseSkip` is the second Tab stop (through `MarketingShell`'s `lead`), the ticker's
  button is the same control, and `prefers-reduced-motion` starts the SMIL timelines paused (CSS cannot stop SMIL). An open mobile menu is a dialog:
  focus moves in, the page behind is `inert`, Tab wraps, focus returns to the toggle.
- A well is a `min-height`, not a `height` (the step-1 graph's is fixed), and a chip row that cannot shrink keeps its padding in px: a reader's larger
  text grows the well instead of clipping it (probed at root font x1.0, x1.5 and x2.0).
- Headings, eyebrows and captions are `text-wrap: balance` (`.lp-card-title`, `.lp-eyebrow`, `.lp-caption`): a block's last line is
  never one short word (the orphan census; a single long word such as "infrastructure." at 320 px cannot be balanced).
- The social card (`app/opengraph-image.tsx`, a route with no file extension) must be listed in BOTH `isPublic` and `isClerkFree` in
  `ui/proxy.ts`, or every signed-out crawler is redirected to sign-in and no link preview ever shows; `next dev` caches the image,
  so restart it before judging an edit.
- Targets are 44 px (`min-h-[44px]`; a touch laptop reports `pointer: fine`, so `.lp-tap` also answers `any-pointer: coarse`).
  Motion respects `prefers-reduced-motion` and the Pause button (`html.lp-paused` stops the CSS loops, SMIL and the terminal's
  typing), and nothing waits on the intro (`html.lp-ready` has a 3 s failsafe).
- Witness a change at 320, 360, 390, 768, 1024 and 1440 px on a PRODUCTION build of a scratch copy (`ui/AGENTS.md`: never
  `next build` in `ui/`): text past its well (measure the text range), tokens torn across lines, targets under 44 px, a chip's
  distance to the limb, caption gaps and the first viewport's colour energy are measured, not eyeballed.
