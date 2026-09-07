# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) from 1.0
onward. Commits use [Conventional Commits](https://www.conventionalcommits.org/),
so this log can be regenerated from history (e.g. with `git-cliff`).

## [Unreleased]

### Added
- **Notifications and an inbox.** An `@mention` in a comment used to fire a
  `pg_notify('comment_mentions', …)` that no process in the codebase ever
  listened for, into a `MSG_MENTION` frame the frontend reserved and never
  received. Mentioning a colleague did nothing they would ever see. Five events
  now write to a `notifications` table — mentions, replies in threads you are
  part of, task assignment, overdue tasks, and documents shared with you — read
  through an Inbox in the sidebar with an unread badge and a `/notifications`
  page. Delivery is a 30-second poll that also refreshes on window focus; the
  table is shaped as an outbox (`emailed_at`) so email can arrive later without
  a migration. Idempotency is a unique index on `(user_id, dedupe_key)` rather
  than application logic, which is what lets the overdue sweep run on every
  replica with no leader election.
- **Two checklist items with identical text assigned to the same person notify
  once.** The notification's dedupe key is content-addressed precisely so that
  reordering a checklist does not re-notify everyone; the cost is that identical
  text collides. The assignee still sees both items on `/tasks`.
- **Self-assignment is not suppressed when you assign yourself by typing.** The
  guard that drops self-notifications needs to know who acted, but the path a
  live edit takes does not carry that: WebSocket updates are persisted with no
  user id, and the channel feeding the reindex worker carries only a document
  id. Editing a checklist item that assigns you a task will notify you about it;
  the guard does work on the import paths.

### Fixed
- **Members whose display name contains a space could not be mentioned when
  opening a thread or replying.** Comment mentions were resolved server-side
  by matching `@(\w+)` against member display names, so `@Christian Hüning`
  captured `Christian`, matched nobody, and silently notified no one. The
  mention picker now sends the user ids it resolved, unioned with whatever the
  regex still matches so a hand-typed second name is never dropped; the regex
  remains as the sole path for comments written before this release. Editing
  an existing comment does not yet carry a `mentions` field, so adding
  `@Christian Hüning` in an edit still notifies nobody — only the create path
  is fixed here.

## [0.5.0] - 2026-09-06

The editor moves from Tiptap 2 to Tiptap 3. The visible result should be nothing
at all — almost all of the work went into keeping the stored document schema
identical to what every existing page was written against — but four things that
were quietly not working are fixed on the way, and a browser that navigates away
mid-request can no longer strand an open transaction on the server.

### Changed
- **The editor runs on Tiptap 3** (2.27.2 → 3.31.3, and the whole `@tiptap/*`
  set with it). The 2.x line is not dead — it still has a `v2-latest` tag and
  picked up a security backport as recently as 2.27.3 — but it is maintenance
  only, so every fix and every new extension now lands on 3.x alone. Two things
  made this more than a version bump. The Yjs binding changed hands:
  `y-prosemirror` is replaced by `@tiptap/y-tiptap`, Tiptap's own fork, and both
  create their plugin key by the same name — so importing `ySyncPluginKey` from
  the wrong one leaves ProseMirror holding `y-sync$` in one place and `y-sync$1`
  in the other, `getState()` silently returns `undefined`, and every comment
  anchor resolves to null with nothing thrown. And `CollaborationCursor` is gone
  in favour of `CollaborationCaret`, which sanitises a colour it cannot parse to
  `transparent` rather than rendering it anyway.
- **The stored schema is deliberately unchanged.** Everything StarterKit 3 adds
  that would have written to a document — `trailingNode`, `listKeymap`, and its
  now-bundled Link and Underline — is disabled, and the snake_case node names
  knot's Rust serialiser expects are preserved. A 0.4.0 document opens on this
  release producing an identical `Y.XmlFragment` and emitting no Y.Doc updates
  at all. The only schema difference any stored page sees is the link `title`
  attribute, which is a fix — see below.
- **The exact `prosemirror-*` pins are gone.** They existed to deduplicate the
  ProseMirror core that `@tiptap/pm` and `y-prosemirror` both pulled in, and
  `y-prosemirror` left with the migration. They had also started doing harm:
  `@tiptap/pm` 3.31.3 asks for `prosemirror-model` ^1.25.11 and
  `prosemirror-view` ^1.42.3, and the overrides held the tree at 1.25.7 and
  1.41.8 — *below* both, silently, because pnpm `overrides` bypass range checks.

### Added
- **Renovate keeps dependencies current** (`.github/renovate.json`). Nothing was
  watching them between manual refreshes. Patch and minor updates merge
  themselves once nextest, vitest, Playwright, clippy and cargo-deny are green;
  majors never open a PR on their own, so a migration like this one stays a
  deliberate choice rather than eight red PRs landing at once. Weekly
  `lockFileMaintenance` refreshes the lockfiles, which is the only route by
  which a patched *transitive* dependency ever arrives. The Rust, pnpm and Node
  versions that each live in three files are grouped so they move atomically.
  CONTRIBUTING.md § Dependencies is the reference.
- **A test baseline for the migration, landed on Tiptap 2 first**, so it is a
  real before/after rather than a story told afterwards.
  `web/src/test/boundEditor.ts` mounts a real editor over a real `Y.Doc` in
  jsdom in about 40ms — the assumption that this needed Playwright is what left
  the gaps. Comment anchors now resolve through the live ySync binding instead
  of a hand-built Map; `schema.test.ts` pins the exact registered-extension list
  and checks attribute parity in both directions against `tools/schema.json`;
  `ydoc.test.ts` asserts the editor writes nothing but the user's own edits.
  Two gaps needed the whole stack: `comment-anchors.spec.ts`, because the
  existing comment spec posted every comment straight to the API with
  `position_y: null` and never exercised anchoring at all, and
  `editor-markdown-roundtrip.spec.ts`, because only paragraph, image and
  `excalidraw_board` had ever travelled editor → Y.Doc → `to_markdown`.
- **`pnpm dedupe --check` gates CI.** Not belt-and-braces: while removing the
  pins above, the ProseMirror core was genuinely split for one run —
  `@tiptap/pm` on `prosemirror-view` 1.41.8 while `@tiptap/y-tiptap` was on
  1.42.3 — and tsc, eslint and all 153 unit tests passed anyway. TypeScript is
  structural, so the two `Node` classes match and nothing else in CI looks at
  resolution.

### Fixed
- **A browser navigating away mid-request could strand a transaction and stall
  the server.** `sqlx`'s `pool.begin()` is not cancellation-safe: drop the
  future after BEGIN has reached Postgres but before `begin()` returns, and sqlx
  never receives a `Transaction` to roll back and keeps no record that one is
  open. The connection goes back to the pool looking clean, and every later
  query handed it silently joins the orphan. One backend was caught holding a
  dozen unrelated documents' work inside a single transaction open for minutes,
  with AccessShare on `doc_updates`, `board_updates` and `comments` plus
  RowExclusive on `sessions`. Nothing ever commits it: it pins a transaction id
  against vacuum and stalls any TRUNCATE or DDL on those tables until the
  process exits. axum drops a handler future exactly this way whenever a client
  disconnects, which is ordinary browser behaviour. `knot_storage::begin` now
  runs the BEGIN on a detached task, so the caller can be cancelled but the
  transaction is always constructed and always dropped — and sqlx's own `Drop`
  rolls it back. Connections also carry `application_name=knot-server` and a 30s
  `idle_in_transaction_session_timeout` as a backstop.
- **`⌘⇧8` and `⌘⇧7` did nothing.** knot renames ProseMirror's nodes to
  snake_case, but renaming a node does not rewrite the options its own commands
  read: BulletList and OrderedList still resolved `itemTypeName` to the
  camelCase `listItem`, so `toggleBulletList()` raised "There is no node type
  named 'listItem'" and the keystroke logged instead of making a list. The
  toolbar buttons pass both names explicitly, which is why only the shortcuts
  were affected.
- **Link titles were dropped from storage.** `tools/schema.json` declares
  `title` on the link mark and both `from_markdown` and `to_markdown` honour it,
  but Tiptap's Link has no such attribute and ProseMirror discards what its
  schema does not declare. `[text](url "title")` survived import and then lost
  its title the first time anyone edited the page — in the CRDT, for every
  reader and for the next export. Nothing backfills: titles already lost this
  way stay lost.
- **Code blocks, Mermaid diagrams and Excalidraw boards ignored wide mode.**
  0.4.0's notes said all three break out into the room; only tables actually
  did. A React node view's outer element is Tiptap's own `div.react-renderer`
  and the wrapper carrying our attributes renders inside it, so
  `.ProseMirror > pre` and `.ProseMirror > [data-testid=…]` selected nothing —
  invisibly, because the generic `.ProseMirror > *` rule still applied and held
  all three at the 712px prose measure. Retargeted at the `node-<type>` class
  Tiptap stamps on the wrapper, and `doc-width.spec.ts` now measures rendered
  widths rather than trusting a selector.
- **Every collaborative session logged "A user uses an unsupported color
  format" once per peer.** `colorFor()` emitted `hsl()`; the caret accepts only
  `#rrggbb`. Noise under Tiptap 2, which rendered the caret anyway — but the v3
  caret extension replaces a colour it cannot parse with `transparent`, which
  would have made every remote caret invisible. Same palette, converted rather
  than re-picked, so nobody's avatar changes colour.

### Security
- **`prosemirror-view` 1.42.3** closes an XSS in clipboard handling: attribute
  validators were not run on attributes arriving via slice context. No advisory
  was ever filed, so no scanner would have surfaced it — the old pins were
  holding knot at 1.41.8. knot was not reachable through it, because Tiptap only
  sets `spec.validate` when an extension declares a validator and knot declares
  none, but being on the patched release closes the exposure the moment that
  stops being true.
- GHSA-cp6q-959q-f8rh (`mergeAttributes()` prototype pollution) is closed by
  `@tiptap/core` 3.31.3. knot was never reachable — ProseMirror builds
  `node.attrs` into a null-prototype object over schema-declared names only —
  and the advisory range no longer flags the shipped version.

### Operators — read before upgrading
- **No migrations, and rollback to 0.4.0 is clean.** Nothing in this release
  changes the database schema.
- **Tell people to reload.** The JS bundle is content-hashed and code-split, so
  a tab left open across the upgrade can request a chunk that no longer exists
  and get `index.html` back instead. A refresh fixes it.
- **A stale 0.4.0 tab strips link titles.** 0.4.0's schema does not declare the
  link `title` attribute fixed above, so if an old tab edits a link run it
  rewrites it without the title, for every peer. The same applies if you roll
  back. It only affects titles, which are new in this release.
- **Comments anchored to the very first character of a document lose their
  inline highlight.** `@tiptap/y-tiptap` rejects an item-based relative position
  resolving to absolute position ≤ 1, which is how 0.4.0 stored an anchor that
  started at the first character. Nothing is deleted — the thread, its body and
  its quoted text all still appear in the sidebar — and it self-heals as soon as
  any content is inserted above the anchor. Comments created on this release are
  unaffected.

## [0.4.0] - 2026-09-05

The document column can be widened to use the window, and several things that
were quietly broken at the edges of any width are fixed.

### Added
- **Fixed / wide document width.** A toggle in the doc header, `⌘⇧F`, a command
  palette action, and Settings → Appearance. One global per-user preference
  (`knot.docWidth`), stamped before first paint so there is no narrow-to-wide
  flash. Not role-gated — width is a reading preference, so viewers get it too;
  hidden below 1024px, where it would buy nothing.
- Wide mode widens the *container*, not the *measure*. Paragraphs hold the same
  712px column and do not reflow a line, while tables, code blocks, Mermaid and
  Excalidraw cards break out into the room. Content box at a 1920px window goes
  712px → 1472px, capped at a 1600px shell so an ultrawide does not sprawl; prose
  stays at 95 characters per line in both modes. The practical win is tables: a
  five-column table goes from 125px cells — six wrapped lines each — to 265px.
- **The comment sidebar reserves space instead of covering the page** at ≥1280px.
  It previously sat on top of a constant 352px of every line at every window
  size, so a wider monitor never helped.
- **Favicon, app icons and a web manifest.** An SVG mark with PNG fallbacks
  (32px, apple-touch, 192/512, maskable), regenerable via `tools/gen-favicon.mjs`.
  Public `/p/{token}` pages link it too — a share link is often someone's first
  sight of knot.

### Fixed
- **The mention and datetime popups could open off-screen.** Both are
  `position: fixed` children of `<body>` placed at the caret's `left` with no
  right-edge clamp. They only ever fit because the fixed column kept the caret
  ~450px from the window edge; in wide mode at 2560px the datetime picker's Apply
  button landed outside the viewport, and being `fixed`, could not be scrolled to.
- **The floating "Add comment" button could give the app a horizontal scrollbar.**
  It was clamped only on the low side, so a selection near the right edge pushed
  it past its host; `<main>` is `overflow-y: auto`, which makes its `overflow-x`
  compute to `auto`, turning that into an app-wide scrollbar. Its "don't cover the
  toolbar" guard also measured against the ProseMirror top rather than the sticky
  toolbar, so it never actually held — the editor scrolls while the toolbar stays
  pinned.
- **Tables with dragged column widths overflowed for readers.** Tiptap only mounts
  its table wrapper when the editor is editable, so view mode — the default — had
  nothing to scroll a pinned table inside. `colwidth` is stored in the CRDT, so one
  collaborator's column drag blew out the page for everyone reading it.
- **The document header overflowed on phones.** At 375px the page scrolled 73px
  horizontally and the last two controls, Save as template and Comments, sat
  off-screen and unreachable: the action row is `shrink-0` with no wrapping, so it
  forced its intrinsic 412px into a 327px content box. Both rows now wrap. This
  also repairs a quieter tablet case — at 768px the title had been squeezed to
  36px beside the action row.
- **Comment highlights hidden behind the sidebar could not be scrolled into view.**
  `scrollIntoView({ block: "center" })` only corrects the vertical axis; insetting
  the document means highlights are never underneath the rail to begin with.

### Changed
- **The e2e suite could wedge the server permanently.** Every spec reset with a
  bare `TRUNCATE ... CASCADE`, which needs ACCESS EXCLUSIVE on tables the CRDT room
  actors keep busy for as long as a document is open. Postgres queues lock requests
  FIFO, so the *pending* TRUNCATE then blocked every query behind it — one reset
  could park the whole server for the rest of the run, recoverable only with
  `pg_terminate_backend` and a restart. `SET lock_timeout` plus a retry fixes it:
  on timeout the statement aborts and stops queueing, so the server is released
  immediately. A full local run is now 50 passed / 1 failed in 2.6 minutes.
  (A row-by-row DELETE also avoids the queue and was measured first — it is worse,
  because without the exclusive lock it races the writers it no longer excludes and
  leaves connections in aborted transactions.)
- 31 copies of the e2e `reset()` helper are now one. They had drifted: several
  omitted `boards`, `share_tokens` and `doc_tasks`, so those tables leaked between
  specs.
- `editor-toolbar` and `history` specs select text with `ControlOrMeta+a`. On macOS
  Chromium binds `Control+A` to "move to start of line", so the selection collapsed
  and Bold applied to nothing — a failure that could only ever reproduce off CI.

## [0.3.0] - 2026-09-04

Markdown documents can be brought into knot — as a file, or by pasting the source.

### Added
- **Import a Markdown file as a page.** "Import Markdown…" sits next to Export on
  the doc page, for editors and owners. 1 MB limit. Importing into a page that
  already has content asks first; the previous version stays in History.
- **Markdown-aware paste.** Pasting Markdown source now produces real headings,
  lists, tables and task items instead of literal `## Heading` text. Left alone
  when the clipboard carries rich text, inside a code block, and on ⌘⇧V.
- `POST /api/docs/{id}/markdown` accepts `?mode=replace`, which clears the page
  before applying. The default stays `append`, so existing callers — including
  create-from-template — are unchanged.

### Fixed
- **Importing over a page that had content duplicated it** instead of replacing.
  The endpoint only ever merged into the live document; its one caller always
  targeted an empty page, so the path was never exercised.
- **Checklists built server-side rendered as plain bullets.** `from_markdown`
  stores `checked` as the string `"true"`/`"false"`, but the editor matched only
  the boolean form. Affected imported files and pages created from a template.
  The Markdown round-trip and the task index were correct throughout; only the
  rendering was wrong.
- A full-document replace is attributed to the user who made it, rather than
  landing in `doc_updates` with a null author.
- `h2` 0.4.15 → 0.4.19 ([RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258),
  unbounded empty DATA frames) and `chacha20` 0.10.1 → 0.10.2 (yanked). Both
  transitive, via hyper and rand.

### Changed
- `pnpm lint` runs in CI, and the backlog it had accumulated unseen is cleared.
  Two of those were real defects: `onClick` and `onPickTemplate` are typed to
  return void, so their inline `async` handlers left promise rejections
  unhandled — a failed request surfaced as an unhandled rejection instead of the
  intended error toast.
- `excalidraw.spec` resets per test instead of per file. Its two "flaky" specs
  were deterministic failures: every test bootstraps through `/setup`, and
  `POST /auth/setup` returns 410 once a user exists, so tests 2 and 3 sat on the
  setup page until they timed out. The playwright job dropped from ~10m to ~5m.

## [0.2.1] - 2026-08-17

Two bugs that CI could not see, and the two tests that now see them.

**The Helm chart could never be installed** — charts `0.1.0` and `0.2.0` both
fail at the very first step of `helm install`. And **restoring a document
snapshot corrupted the document for every connected client**, appending the
restored text to the existing content instead of replacing it.

### Fixed
- **Restoring a snapshot merged instead of replacing.** `ReplaceWithMarkdown`
  clears the document fragment and applies the restored content in a single
  transaction, but it broadcast and persisted the caller's `update_bytes` —
  which encodes only the insertion. Peers therefore received the insert without
  the delete and kept what they already had:

  ```text
  expected: "First version of the doc."
  actual:   "Completely different content.First version of the doc"
  ```

  Because the _persisted_ update was also missing the deletion, this was a
  data-integrity bug and not merely a display glitch: replaying the update log
  reproduced the merged content. The room now captures what the transaction
  actually produced via `observe_update_v1` — the same pattern
  `PatchTaskChecked` already used — and persists and fans that out.

  The existing `replace_with_markdown_swaps_content` test could not catch this:
  it asserted on the room's own document, which was always correct. The new
  `replace_broadcast_replaces_peer_content` asserts on the frame a peer
  receives, applied to a peer holding the pre-restore state.

  Affects every release with document history: `0.1.0` and `0.2.0`.
- **The chart could never be installed.** The `pre-install`/`pre-upgrade`
  migrate Job injected `KNOT_DATABASE_URL` but not `KNOT_SESSION_KEY`. The binary
  loads and validates its entire config _before_ dispatching the subcommand, and
  validation rejects an empty session key — so the hook exited 2 with
  `KNOT_SESSION_KEY is required` and **every** `helm install` and `helm upgrade`
  failed before reaching the Deployment:

  ```text
  Error: INSTALLATION FAILED: failed pre-install: resource Job/knot-migrate
         not ready. status: Failed, message: Job Failed. failed: 1/1
  config: invalid: KNOT_SESSION_KEY is required (set it in every environment)
  ```

  `migrate` never reads the session key; it only has to survive validation. The
  Job now receives it from the same Secret the Deployment uses, honouring
  `session.existingSecretName`/`existingSecretKey`.

  The bug survived two releases because `ct install` is disabled in chart CI, so
  nothing had ever deployed the chart — `helm lint` cannot see a hook that fails
  at runtime.

### Added
- **Helm upgrade test** (`.github/workflows/helm-upgrade.yaml`). Installs the
  previous _published_ release — chart pulled from ghcr and the real image, not
  the working tree — seeds a document through that release's own API, runs
  `helm upgrade` to the working tree, and then asserts:

  1. `/api/version` changed — the binary actually swapped
  2. the session cookie minted by the **old** version still authorises — the
     session format and signing key survived
  3. the seeded document still reads back — schema and migrations survived

  Runs on chart, migration and `Dockerfile` changes, weekly against `main`, and
  on demand. This is what caught the migrate Job bug.
- `replace_broadcast_replaces_peer_content`, a room-level test asserting on the
  bytes a peer actually receives from a restore rather than on the room's own
  document. Reverting the fix turns it red with the exact production symptom
  (`<paragraph>Replaced Content</paragraph><paragraph>Hello World</paragraph>`),
  while the pre-existing test stays green.

### Changed
- `history.spec` no longer restores an arbitrary snapshot. It selected
  `snapButtons.last()` — the oldest — which with `KNOT_SNAPSHOT_EVERY_N=1` is
  the complete V1 only when every keystroke lands in one persisted batch: true
  on a fast machine, false on a loaded runner, so the spec restored a prefix and
  asserted on the full string. Its preview check only required `"First version"`,
  which any prefix satisfies, so the failure surfaced later and read as a restore
  bug. It now pins the newest snapshot that contains all of V1 and asserts the
  full string in both places.

### Operators
- **Upgrading from chart `0.2.0`.** If you worked around the broken hook with
  `--set migrations.enabled=false`, drop that override — `0.2.1` runs migrations
  itself again. Nothing else about your values file changes.
- **Installing fresh.** `helm install` now works with defaults; no manual
  `/knot-server migrate` step is needed.
- The stray `0.3.4` chart package has been deleted from ghcr, so
  `helm install oci://ghcr.io/opendefensecloud/charts/knot` without `--version`
  resolves to the newest real release instead of the mis-versioned `v0.1.0`
  chart. The `--version` advice in the 0.2.0 notes below is no longer required.
- The Prometheus `route` label change noted under 0.2.0 still applies if you are
  coming from `0.1.0`.

## [0.2.0] - 2026-08-16

Maintenance release: a full dependency refresh across every surface, clearing
all outstanding advisories. No user-facing features changed, but the operational
notes below need reading before rollout.

### Security
- `cargo deny check` passes again (previously failing): `ammonia` 4.1.4 fixes
  two XSS issues (RUSTSEC-2026-0193 mXSS via MathML `annotation-xml`,
  RUSTSEC-2026-0213 XSS via SVG `animate`/`set`), plus `anyhow` 1.0.104
  (RUSTSEC-2026-0190), `crossbeam-epoch` 0.9.20 (RUSTSEC-2026-0204) and the
  yanked `spin` 0.9.8.
- `pnpm audit` goes from 53 advisories (1 critical, 26 high, 22 moderate, 4 low)
  to **zero** in `web/`; `e2e/` was and stays clean.
- Dropped `rustls-pemfile` (unmaintained, RUSTSEC-2025-0134) in favour of the
  `PemObject` trait from `rustls-pki-types`, which was already in the graph.
- The pnpm `overrides` moved to `web/pnpm-workspace.yaml`; pnpm ≥ 11 no longer
  reads the `pnpm` field in `package.json`. Their floors were also stale — the
  `nanoid` pin was `^3.3.8` where the advisories require `>= 3.3.18`. `dompurify`
  and `lodash-es` are now patched too; both were previously documented as having
  no available fix.
- The pinned package manager itself, pnpm 9.0.0, carried 24 open advisories with
  no fix anywhere in the 9.x line. Now pnpm 11.22.0.
- Dex 2.41.1 → 2.45.1 in the dev compose stack (GHSA-7qjx-gp9h-65qj).

### Fixed
- The Helm chart's default `image.repository` pointed at
  `ghcr.io/christianhuening/knot` — a real, populated registry belonging to a
  different lineage — while the release workflow publishes to
  `ghcr.io/opendefensecloud/knot`. A default `helm install` therefore ran an
  unrelated image and _appeared to work_. It now points at the published
  repository.
- `Chart.yaml`'s `version: 0.3.4` / `appVersion: "0.1.7"` were inherited from
  that same unrelated lineage and corresponded to no tag in this repo. Both are
  now `0.0.0` and documented as inert placeholders, since `release.yaml` forces
  each from the git tag at package time. `ct.yaml` sets
  `check-version-increment: false` to match.
- Corrected `deny.toml`: the RUSTSEC-2023-0071 entry described the path as
  `openidconnect -> oauth2 -> rsa`, but `oauth2` declares no `rsa` at all — it
  is a direct dependency of `openidconnect`.

### Changed
- Rust toolchain 1.96.0 → 1.97.1; declared MSRV corrected 1.80 → 1.94 (the real
  floor, set by sqlx 0.9).
- axum 0.7 → 0.8, sqlx 0.8 → 0.9, yrs 0.21 → 0.27, OpenTelemetry 0.27 → 0.32,
  rand 0.8 → 0.10, zip 2 → 8, rust-s3 0.36 → 0.37, tower-http 0.6 → 0.7,
  pulldown-cmark 0.12 → 0.13, ipnetwork 0.20 → 0.21,
  metrics-exporter-prometheus 0.16 → 0.18, tokio-postgres-rustls 0.13 → 0.14.
- React 18 → 19, Vite 5 → 7, Vitest 2 → 4, lucide-react 0.460 → 1.31.
- Node 22 → 24 across the Dockerfile, CI and the Nix dev shell; zig 0.13 → 0.16
  in the release image; base images and GitHub Actions refreshed to current
  majors.
- Removed dependencies that nothing imported: `proptest`, `testcontainers` and
  `testcontainers-modules` (declared but absent from `Cargo.lock`), plus
  `y-websocket`, `@tiptap/extension-bubble-menu` and `@tiptap/extension-mention`.

### Operators — read before upgrading
- **Prometheus label values change.** `knot_http_requests_total` and
  `knot_http_request_duration_seconds` carry a `route` label taken from axum's
  `MatchedPath`. axum 0.8 changed path-parameter syntax, so values move from
  `/api/docs/:id` to `/api/docs/{id}`. The bundled Grafana dashboard and
  PrometheusRule do not select on `route` and are unaffected, but any external
  dashboard, alert or recording rule keyed to the old form will silently stop
  matching.
- **Pin the chart version when installing.** The `v0.1.0` release published its
  chart as `0.3.4` (the bug fixed in b45a8bb). OCI registries have no `latest`
  for charts, so `helm install` without `--version` resolves the highest semver
  — still `0.3.4`. Pass `--version 0.2.0` explicitly, or delete the stray
  `0.3.4` package version.

### Deferred
Tailwind 4, TypeScript 7 (blocked: `@typescript-eslint` peers `<6.1.0` and this
repo relies on type-aware linting), Tiptap 3, jsdom 30 (needs Node ≥ 22.22.2),
Vite 8 (Rolldown/Oxc), and `reqwest` 0.13 (pinned by `openidconnect`).

## [0.1.0] - 2026-06-04

First tagged release. Feature-complete for single-workspace teams.

### Added
- Real-time collaborative document editing on Yjs/yrs CRDTs over a single
  WebSocket protocol; cross-pod fan-out via Postgres `LISTEN/NOTIFY`.
- Local (Argon2id) and OIDC authentication; session + CSRF cookies.
- Document tree, ACL grants, public share links, comments with @mentions,
  reactions, tasks/checklists with due dates, full-text + prefix search.
- Excalidraw boards, Mermaid diagrams, tables, image/file attachments,
  Markdown import/export, document templates and history.
- Observability: structured logging, OTLP traces, Prometheus metrics.
- Helm chart with migrate hook, NetworkPolicy, ServiceMonitor, PrometheusRule,
  and multi-arch (amd64 + arm64) scratch image.

[Unreleased]: https://github.com/opendefensecloud/knot/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/opendefensecloud/knot/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/opendefensecloud/knot/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/opendefensecloud/knot/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/opendefensecloud/knot/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/opendefensecloud/knot/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/opendefensecloud/knot/releases/tag/v0.1.0
