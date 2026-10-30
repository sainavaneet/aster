# Aster visual redesign: phase 0 audit and design spec

| | |
|---|---|
| Status | Phase 0 (audit and spec). No product code. |
| Branch | `feat/visual-redesign` |
| Authors | design-sonnet (design), design-sol (feasibility review, see [section 13](#13-feasibility-review-design-sol)) |
| Date | 2026-10-29 |
| Scope | The terminal UI: `src/ui/**`, `src/client/shell/**`, the Orbit projection in `ClientShellSnapshot`, and the `aster orbit activity` CLI view. |

## 0. Summary

Aster is already a capable agent workspace, but its chrome speaks in one voice for everything: a flat list of
dots and labels. The Orbit team (lead, roles, members, relays, parallel steps) exists in the data but not on
screen. The redesign makes six things readable at a glance, without adding noise:

1. **Who is working on what.** Every member row and pane header carries role, member, kind, model, task title.
2. **What needs me.** One lane, always first, always the same glyph and color: blocked, awaiting approval.
3. **What just finished.** A short `done` lane that decays after ten minutes (the rule that exists today).
4. **How a task moves.** Relay progress (`step 2/3`), parallel progress (`parallel 1/2`), `integrating`, and a flow view that shows lead → role → member steps with the handoff note between them.
5. **Where the activity is.** Per-role activity and an attention summary in the tab bar; slim worker headers on pane borders.
6. **Consistency.** One glyph set, one color-role table, one age formatter, shared by sidebar, tab bar, pane headers, toasts, status line and the CLI.

The visual language is **calm by construction**: shape carries state (color is a second channel, never the
only one), color is reserved for state, one accent marks focus, motion appears only where it carries meaning
and is absent under `ui.animations = "off"`, nothing moves when the team is idle.

Everything new is behind `ui.style = "modern" | "classic"` (default `modern`). `classic` renders exactly
what ships today; this spec defines how that is enforced ([section 9](#9-keeping-classic-pixel-identical)).

Design decisions worth reading first:

- **The new chrome never changes geometry.** Pane sizes, sidebar width, tab bar height and the PTY sizes are
  identical in both styles, so switching style never resizes a terminal. Modern lives inside the rectangles
  classic already allots.
- **Pane headers are painted by the client**, onto the border row the server already draws, from data the
  client already has. No server surface change, no new wire message ([section 6.4](#64-pane-headers-are-client-painted)).
- **The directory split is the performance rule.** Code that allocates (models) and code that draws live in
  different directories, and the hot-path architecture test is extended to forbid allocation in the draw
  directory ([section 6.5](#65-enforcing-pure-render)).
- **New server data is three optional fields** (`model`, `steps`, `step_total`), following the existing
  `server_now_ms` precedent ([section 7](#7-data-contract-additive-fields-only)).

## 1. Goals and non-goals

Goals: the six outcomes above; light and dark themes; `ui.animations` full/reduced/off; mobile width; the
collapsed sidebar; mouse and keyboard navigation unchanged; 80, 120 and 200 columns; ASCII fallback.

Non-goals: changing layout geometry, keybindings of existing actions, the server protocol's existing fields,
terminal content rendering (pane surfaces), agent detection, or the multi-endpoint sidebar's structure.
No new crates are needed (everything is `ratatui` cells and `unicode-width`, both already dependencies).

## 2. Baseline (phase 0)

Measured on this checkout (macOS arm64, release build) with
`just bench-render-scale`, 120x40, 40 samples after 5 warmups. Raw output is kept at the end of this file.
Every later phase re-runs the same recipe and reports the same rows.

**Combined pipeline** (server pane surface + client shell composition), median in microseconds:

| scenario | 1 pane | 15 panes | 50 panes | 15 vs 1 | 50 vs 1 |
|---|---:|---:|---:|---:|---:|
| background workspaces (one pane each) | 409 | 429 | 435 | 1.05x | 1.06x |
| active panes (one workspace) | 407 | 477 | 543 | 1.17x | 1.33x |

**Client shell composition** alone (the part this redesign touches), median / p95 in microseconds:

| scenario | 1 pane | 15 panes | 50 panes |
|---|---:|---:|---:|
| background workspaces | 156 / 179 | 171 / 182 | 176 / 191 |
| active panes | 149 / 156 | 163 / 182 | 165 / 198 |
| token rules, background, 16 rules/token | 153 / 160 | 179 / 190 | |
| token rules, active, 16 rules/token | 152 / 163 | 171 / 177 | |

**Server pane surface** alone (pane borders and titles are drawn here, see P8): background 251 / 255 / 256,
active 257 / 314 / 379 for 1 / 15 / 50 panes.

**Snapshot encoding** (populated agent snapshots + completions + JSON framing, one client): 1 pane 6 us,
15 panes 49 us background / 31 us active, 118 panes 384 us / 245 us.

**Known failure, pre-existing:** `server::headless::tests::native_graphics::native_file_render_scale_profile`
panics at `src/server/headless/tests/native_graphics.rs:1061`
("source profile requires working CoW retention, not decoded fallback"), so `just bench-render-scale` exits
101 on this machine after printing every other profile. It is environmental (no copy-on-write retention) and
unrelated to the UI. The other six profiles pass. Reports quote the six passing profiles and note the failure.

**Gap in the baseline that phase 2 must close.** `render_scale_benchmark.rs` builds snapshots without an
`orbit` projection, so no existing profile exercises the team sidebar, the work board, or any Orbit-driven
chrome. Phase 2 adds an Orbit-populated scenario ([section 10](#10-performance-plan)) and records its
classic numbers *before* any modern code lands.

## 3. Audit: what the UI is today

### 3.1 Surface map

| Surface | Where it is drawn | Notes |
|---|---|---|
| Sidebar frame, workspace list | `src/client/shell/sidebar.rs`, `src/ui/sidebar.rs`, `src/ui/sidebar/tokens.rs` | Two sections split by `sidebar_section_split`. Separator `│` in `render_sidebar_background` (`render.rs:16`). |
| Agent list (flat) | `src/client/shell/agent_sidebar.rs` | `render_agent_row` (`:395`), token-driven rows. |
| Orbit team tree | `src/client/shell/team_sidebar.rs` (model + line render), `team_rows.rs` (cached rows) | Active only with one endpoint and `ui.sidebar_team` (`active_team_sidebar`). |
| Work board | `src/client/shell/work_board.rs` | Active and recent assignments below the tree. |
| Collapsed sidebar | `sidebar.rs:47` `render_collapsed_sidebar` | One glyph per workspace; `collapsed_entries` for panes. |
| Tab bar | `src/client/shell/tabs.rs` | Pill per tab, spinner at column 0, sweep animation. Right status from `tab_bar_right`. |
| Pane borders and titles | **server**: `src/ui/panes.rs` (`render_pane_borders`, `render_pane_border_titles`) via `src/ui/tab_surface.rs` | Baked into the pane surface the client blits (`shell.rs:250` `blit_pane_surface`). |
| Mode bar (status line) | `src/client/shell/render.rs:31` `render_mode_bar` | Drawn only outside Terminal mode. |
| Toasts | `src/client/shell/notifications.rs`, `src/ui/status.rs` | Card with a static `●`. |
| Overlays | `src/client/shell/overlays.rs` (`panel`, `popup` helpers), `overlay_input.rs` | Help, navigator, settings, menus, rename, confirm, worktrees, release notes. |
| Mobile | `src/client/shell/mobile.rs` | Own item styles. Below `mobile_width_threshold` (64). |
| Animation | `src/client/shell/animation/{clock,phase,notification,shell_state}.rs` | Presentation state only; render samples one `AnimationFrame`. |
| Theme | `src/app/state.rs:35` `Palette` (19 flat colors); `src/config/theme.rs` (18 themes) | |
| Orbit data | `src/protocol/wire.rs:964` `OrbitShellView`, built in `src/app/orbit_notifications/shell_view.rs` | Rebuilt at most every 250 ms, published only when it changes. |
| Orbit CLI view | `src/cli/orbit_activity.rs` | Server JSON → text tables. |

Rendering is split in two processes' worth of code: the server draws the pane surface (terminal content,
borders, border titles) and the client composes it with the sidebar, tab bar, overlays and toasts
(`composition.rs`). The Orbit snapshot, per-pane geometry (`PaneSurfacePane { rect, inner_rect, .. }`) and
the animation clock are all on the client side. That is what makes client-painted pane headers possible.

### 3.2 Problems, with evidence

**Information the user needs is missing**

- **P1. No model, no step, no relay on screen.** A team row is `name` + `kind` (`team_sidebar.rs:115` `member_row`, drawn at `:473`).
  `OrbitShellMember` has no model (`wire.rs`), although roster members carry one (`orbit_team/roster.rs:57`).
  `OrbitShellActiveTask` has no steps, although the server tracks them (`orbit_notifications/steps.rs`,
  `Step { state, handoff, parallel, integrate }`) and renders `step 2/3` and `parallel 0/2 done` for the lead
  (`orbit_notifications/handoff.rs:126` `orbit_step_line`). A relayed or parallel task looks like any other.
- **P2. The work board is a flat list.** Blocked tasks sort first and nothing else is grouped (`work_board.rs:122`).
  There is no queued notion, although `state == "assigning"` exists on the wire. Approval is an accent-colored
  `⚑ ` (`work_board.rs:27`), the same color used for "focused", so "needs you" and "selected" are one color.
- **P3. Task identity is the 8-character id tail** (`short_id`, `work_board.rs:59`), 9 of the 25 columns of a
  default sidebar, while the human-readable title sits on a second row in the dimmest text.
- **P4. Right-aligned facts vanish silently.** `put_right_parts` (`work_board.rs`) draws state and age only if
  they fit after the name, otherwise draws nothing, so at the default width `working 12m` disappears for any
  role name over a few characters.
- **P5. Group state is under-reported.** A collapsed group header shows one icon for the most urgent member
  (`team_sidebar.rs:533`), no counts, no pending-approval signal.

**Visual language**

- **P6. State is encoded mostly by color.** In the default `dots` style working, blocked and done are all `●`
  (`shell.rs:189`); only the color differs. Two yellow/peach themes (Latte: yellow `223,142,29`) put working and
  attention within a few shades of each other. Not usable for color-blind users or monochrome terminals.
- **P7. Color semantics mislead.** `status_color` (`shell.rs:229`) paints *idle* green and *done* teal.
  Green on a resting agent reads as success; the transient `done` state is the one that earned it.
- **P8. No semantic roles in the theme.** `Palette` is 19 flat colors (`app/state.rs:35`); every call site picks
  `overlay0`/`overlay1`/`subtext0` ad hoc. Contrast is not validated: for Latte, yellow on `239,241,245`
  is roughly 2.3:1 by our estimate (to be verified by the phase 1 contrast test).
- **P9. No ASCII fallback anywhere.** `▾ ▸ ⚑ ● ○ ◐ ✓ × · │` and the braille spinner are hardcoded in 20+ places;
  `ui.status_indicators` only toggles dots vs symbols. Ambiguous-width glyphs misalign on CJK-ambiguous terminals.
- **P10. The loudest motion is on the most persistent state.** A blocked task pulses the **whole row background**
  every 1.6 s (`work_board.rs` `render_task`, `team_sidebar.rs` `row_background`) for as long as it stays blocked.

**Tab bar, pane borders, status**

- **P11. The tab bar says almost nothing.** A tab is a padded label; the only signals are a spinner in column 0
  and a 640 ms sweep (`tabs.rs:129-169`). Nothing for "something here needs you", counts, or roles.
- **P12. Pane borders carry no worker identity.** Border color is focus / not focus
  (`panes.rs:592`); the title is the agent label only when `show_agent_labels_on_pane_borders` is on (default false).
  In a four-worker relay the panes are visually identical.
- **P13. No persistent attention summary.** The mode bar exists only outside Terminal mode (`render.rs:41`), so
  while typing there is no always-visible "1 needs you".
- **P14. Toasts are one shape.** `render_notification_card_with_phase` draws a static `●` (`notifications.rs`)
  whatever happened. Finished, blocked and approval look alike.
- **P15. Two width bugs in `src/ui/status.rs`.** `copy_feedback_rect` sizes the toast with
  `feedback.message.len()` (`:25`) and the config diagnostic uses `text.len()` (`:110`): byte length, not display
  width, so a non-ASCII message gets a box too wide. (Proposed as a separate `fix:` commit, see risk R9.)

**Performance smells the redesign must not copy** (found while reading; the current code is within budget,
these are warnings for new code)

- **P16. Per-frame allocation in render paths.** `render_agent_row` clones the token rows, builds a `vec!`, calls
  `" ".repeat(indent)` and collects spans per row per frame (`agent_sidebar.rs:450-474`); `render_tab_bar`
  `format!`s every tab label per frame (`tabs.rs:121`) and runs `snapshot.agents.iter().any(..)` per tab per
  frame (`tabs.rs:129-134`), which is tabs x agents in the render loop.
- **P17. Server border rendering scales with panes x cells.** `render_pane_borders` builds a `HashMap` of border
  cells per surface render and, for each cell, scans every pane (`panes.rs:575-596`); titles allocate a `String`
  per pane (`render_pane_border_titles`). This is the server-side "active panes" 1.22x at 15 and 1.47x at 50 in
  the baseline above. The redesign adds nothing here (headers are client-painted).
- **P18. The architecture test does not cover the files this redesign touches.**
  `scripts/test_ui_hot_path_architecture.py` scans `src/ui.rs`, `src/ui/**` and `render_stream.rs` only;
  `src/client/shell/**`, where the sidebar, tab bar and toasts render, is outside it.

**Structure**

- **P19. Navigation duplicates layout.** `TeamSidebar::row_index_of` (`team_sidebar.rs:291`) recomputes row order
  independently of the cached `TeamRows::lines`. Any new row kind (spacers, lane headers) would silently
  desynchronize keyboard scrolling.
- **P20. Duplicated formatters.** Age text exists twice with different units: `age_text` (`work_board.rs:212`,
  `s/m/h/d`) and `age` in `cli/orbit_activity.rs` (`s/m/h`).
- **P21. Mobile and multi-endpoint have their own renderers** (`mobile.rs`, `endpoint_sidebar.rs`) with their own
  item styles, and the team tree is disabled with more than one endpoint.
- **P22. Large modules.** `state.rs` 1951 lines, `mouse.rs` 2421, `team_sidebar.rs` 635. New code must not grow them.

## 4. The visual language

### 4.1 Principles

1. **Shape first, color second.** Every state has a distinct glyph shape; color reinforces it. Remove all color and the screen still reads.
2. **Color is for state.** One accent (focus). Role identity is a thin tint on a 1-cell gutter. Everything else is neutral text tones.
3. **Quiet by default, loud by exception.** An idle team renders in neutral tones. The only saturated things on screen are things that are working, blocked, or need a decision.
4. **Needs-you outranks everything.** It is the first lane, the first tab marker, the first chip in every summary, and the only thing allowed to pulse.
5. **Density follows width.** The same row drops fields in a fixed order instead of wrapping or overlapping. Nothing is ever silently dropped without a rule (fixes P4).
6. **One vocabulary.** The same glyph means the same thing in the sidebar, tab bar, pane header, toast, flow view and CLI.
7. **No layout shift.** Fields that change value (state glyph, counts, age) occupy fixed-width cells, so a state change repaints cells and never moves text.

### 4.2 Hierarchy (type scale)

There is no font size, so hierarchy is built from tone and weight. Five levels, all existing `Palette` roles:

| Level | Used for | Style |
|---|---|---|
| L1 section label | `SPACES`, `TEAM`, `WORK` | `overlay0`, bold, uppercase constant |
| L2 group / lane header | role name, lane name | `subtext0`, bold (role name in `text` when it holds the focused pane) |
| L3 row primary | member name, task role | `text` if focused or lead, else `subtext0` |
| L4 meta | kind·model, age, counts | `overlay0` |
| L5 detail | task title, handoff note | `overlay1` |

### 4.3 Spacing and geometry

- 1-column left gutter. Group header at indent 1, member/task row at indent 3, detail row at indent 5.
- A 1-column **role gutter** `▌` at column 1 of a role card (header and members), tinted per role (4.5). It replaces the `▾ ▸` chevron for roles; collapsed state is shown by the first member row disappearing plus a `▸` after the counts.
- One blank spacer row between TEAM and WORK, and between lanes **only** when `height >= 30` rows; otherwise lanes touch. No spacer is ever emitted below the last row.
- Density tiers, from the **sidebar content width** (`sidebar_width - 1`; default 25, range 17 to 35):

| Tier | Content width | Member row | Task card |
|---|---:|---|---|
| narrow | < 22 | glyph, name | glyph, role, age; no title row |
| normal | 22 to 31 | glyph, name, kind (model dropped if it does not fit) | 2 rows: glyph, role, age bar, age / title; +1 progress row for relay or parallel |
| wide | >= 32 | glyph, name, kind·model | as normal, plus `next` hint on the progress row and the short task id |

- Vertical priority when the list overflows its section: `needs you` and its tasks are never dropped; then
  `working`, then `queued`, then `done`. A truncated lane keeps its header and shows `+N` on it.
- Field drop order inside one row, rightmost goes first: model, age bar, age, kind, role name (tint stays), title (truncated to a minimum of 8 cells, then dropped). Truncation uses `…`.

### 4.4 Glyph set

All glyphs are single-cell under `unicode-width`. `ui.glyphs = "ascii"` swaps every unicode glyph for its
fallback. Spinner: the existing braille set `⠋⠙⠹⠸⠼⠴⠦⠧` (`ui/sidebar.rs:19`), ASCII `|/-\`.

| Concept | Unicode | ASCII | Notes |
|---|---|---|---|
| working | `◐` (spinner while animating) | `*` (spinner `\|/-\`) | yellow |
| idle | `○` | `o` | neutral, not green (fixes P7) |
| blocked | `▲` | `!` | red |
| needs approval | `⚑` | `?` | peach, the "your turn" glyph |
| done | `✓` | `+` | green |
| failed / cancelled | `✗` | `x` | red |
| queued / pending | `◌` | `.` | neutral |
| exited / unknown | `·` | `-` | neutral, dim |
| group open / closed | `▾` / `▸` | `v` / `>` | existing |
| role gutter | `▌` | `\|` | role tint |
| lead | `◐`/`○`/`▲` state glyph, label `lead` | same | no special glyph |
| relay node done / active / pending / failed | `●` `◐` `○` `✗` | `#` `*` `.` `x` | step strip |
| relay connector, progress filled | `━` | `=` | |
| connector / progress empty | `─` | `-` | |
| handoff arrow | `▶` / `╰▶` | `>` / `\\>` | mauve, flow |
| parallel | `‖`, tree `┬ ├ └ │` | `\|\|`, `+ + \\ \|` | teal |
| integrate | `⊕` | `+` | teal |
| separator | `·` | `.` | |
| ellipsis | `…` | `~` | |
| tab role chip filled / empty | `▪` / `▫` | `#` / `.` | per role, tinted |
| key hint arrows | `↑ ↓ ← →` | `^ v < >` | |

Kind is text (`claude`, `codex`, `gemini`), never a brand glyph: no font assumptions.

Scope of `ascii`: it covers all client-drawn chrome. Box borders and dividers of **panes** are drawn by the
server (`line_cell_symbol`, `panes.rs`) and keep their box-drawing characters; extending `ascii` to them is a
phase 6 item (R4).

### 4.5 Color roles

New roles are **derived from the existing `Palette`**, once, at palette build. No new theme keys, so no change to
`theme.custom` or the config reference. `ChromeRoles::derive(&Palette)` lives in `src/ui/chrome/roles.rs`.

| Role | Dark source | Light source | Used for |
|---|---|---|---|
| `state_working` | `yellow` | `yellow`, contrast-adjusted | working glyph |
| `state_blocked` | `red` | `red` | blocked, failed |
| `state_approval` | `peach` | `peach`, contrast-adjusted | approval marker, needs-you lane header |
| `state_done` | `green` | `green` | done glyph |
| `state_idle` | `overlay1` | `overlay1` | idle, queued, unknown (neutral) |
| `focus` | `accent` | `accent` | focused row text, focused border, focused pane header |
| `flow` | `mauve` | `mauve` | handoff arrows, relay connectors |
| `parallel` | `teal` | `teal` | parallel and integrate marks |
| `role_tint[0..4]` | `accent`, `mauve`, `teal`, `overlay1` | same | role gutter and role chips, cycled by role index; never used for state |
| `text_strong / body / muted / subtle` | `text / subtext0 / overlay0 / overlay1` | same | L3 / L2 / L4 / L5 |
| `rule` | `surface_dim`, or `surface1` on `Reset` backgrounds | same | separators, empty bar cells |
| `row_focus_bg`, `row_flash_bg` | `active_row_bg`, `blend(sidebar_bg, surface0, t)` | same | focused row, recency highlight (only when blendable) |

Rules:

- **Contrast.** `derive` runs `ensure_contrast(fg, bg, 3.0)` for every state role against `panel_bg` and `sidebar_bg`,
  nudging toward `text` in small steps. Colors that are not `Rgb` (the `terminal` theme, `Reset`) cannot be
  measured; they fall back to the base `Palette` color plus `BOLD`. A unit test iterates all 18 `THEME_NAMES`
  and fails below 3:1 for any RGB pair.
- **Never rely on a background fill for meaning.** `sidebar_bg` is often `Reset`. Fills (focus row, recency flash)
  are a bonus when blendable (`animation::blend_color` already switches at the midpoint for non-RGB).
- **Derive once, recompute on theme change.** The theme can switch at runtime (`theme.auto_switch`).
  `ClientShellConfig` owns `ChromeRoles` and is rebuilt wherever `palette` is replaced (`config.rs:365` path), so
  roles cannot go stale (R6).
- Classic never reads `ChromeRoles`.

### 4.6 State vocabulary and "needs you"

`StateKind` (client-side enum, precomputed from the wire strings, unknown strings opaque):

`Working, Idle, Blocked, Approval, Done, Failed, Queued, Exited, Unknown`

- Member state comes from `OrbitShellMember.state`; supervision phases (`pending`, `starting`, `relaunching`) map to `Queued`; `exited`, `gave_up`, `unsupported`, `unsupervised` map to `Exited`.
- Task lane (`Lane`) is a pure function of the wire task, computed at model build:
  `needs_you` = `blocked || awaiting_approval`; `queued` = `state == "assigning"`; everything else active is `working`
  (an active task whose worker reports `idle` is shown with the idle glyph inside `working`: an honest "nothing is moving"); resolved tasks are `done` for ten minutes (`RECENT_VISIBLE_MS`), failures drawn with `✗` in the same lane.
- Sort inside a lane is fixed at build time (no time dependence, so the row cache is not invalidated by the clock): needs-you by oldest first, working by oldest first, queued by order, done newest first.

### 4.7 Motion

Motion only where it carries meaning. All of it reuses the existing clock (`animation/`), keyed by
`(endpoint, pane id)`, sampled once per frame. `ui.animations = "off"` never schedules a tick; `"reduced"` keeps
only one-shots.

| Effect | Where | full | reduced | off |
|---|---|---|---|---|
| working spinner | any working glyph (sidebar, tab chip, pane header, flow) | braille, 12.5 Hz, **one shared phase** so all spinners move in unison | static `◐` | static `◐` |
| needs-you pulse | glyph foreground only (not row background), 1.6 s | on | static | static |
| state-change flash | glyph brightens 320 ms | on | on | none |
| recency highlight | row background (or bold glyph when not blendable), 2 s decay | on | on | none |
| handoff sweep | flow view connector `╰▶`, 640 ms, one-shot when a step changes | on | none | none |
| toast slide / fade | existing | on | fade only | none |
| age text and age bar | never animated; they repaint when something else renders | | | |

Budget rules: at most one spinner family and one pulse family animate at once; nothing moves for an idle
team; no effect changes layout. P10 is fixed by moving the pulse from the row background to the glyph.

Age and time: no new timers in phases 1 to 5. Ages and bars are computed per frame from the endpoint clock
(`orbit_frame_unix_ms`) like today, so they refresh whenever a frame is drawn. A coarse refresh deadline is a
phase 6 question (R8).

## 5. Screens

Widths: the sidebar defaults to 26 columns (25 content + the `│` separator, `sidebar_width = 26`); the
120-column mockups use a 31-column sidebar, which is a normal drag. Panes are shown with borders on (the default
for split panes). All mockups use the same scene: a lead (Polaris), a `builders` role with three members, a
`reviewers` role with one, a relayed task in progress, a parallel task in progress, one approval pending, one task
queued, one done.

### 5.1 Full screen, 80 columns

```text
 SPACES                  │ main ▪▪ ◐2   tests ◐   docs   +             ▲1 ⚑1 ◐3
 ▸ aster               ◐ │┌▌ada ◐ fix flaky auth… ──┐┌▌cy ▲ migrate sess… ─⚑4m ┐
   api-gateway         ▲ ││$ cargo nextest run      ││Allow Bash(rm -rf        │
   docs                ○ ││  Running 214 tests      ││sessions/old) ?          │
─────────────────────────││  PASS auth::refresh     ││                         │
 TEAM                 ▲1 ││  FAIL auth::token_race  ││ ❯ 1. Yes                │
 ◐ Polaris          lead ││                         ││   2. No                 │
 ▌ builders        ◐1 ▲1 ││> fixing the race…       ││                         │
 ▌ ◐ ada     claude·opus ││                         ││                         │
 ▌ ○ bo      codex·gpt-5 ││                         ││                         │
 ▌ ▲ cy           gemini ││                         ││                         │
 ▌ reviewers          ○1 ││                         ││                         │
 ▌ ○ rae      claude·son ││                         ││                         │
                         ││                         ││                         │
 WORK           1⚑ 2◐ 1○ ││                         ││                         │
 ▲ needs you  1          ││                         ││                         │
   ⚑ builders         4m ││                         ││                         │
     migrate session…    ││                         ││                         │
 ◐ working  2            ││                         ││                         │
   ◐ builders ━━─    12m ││                         ││                         │
     fix flaky auth…     ││                         ││                         │
     ●━◐━○  step 2/3     ││                         ││                         │
   ◐ reviewers ━──    3m ││                         ││                         │
     review checkout…    ││                         ││                         │
     ‖ parallel 1/2      ││                         ││                         │
 ○ queued  1             ││                         ││                         │
 ✓ done  1               ││                         ││                         │
                         │└─────────────────────────┘└─────────────────────────┘
```

Reading the screen: the tab bar's right chip says `▲1 ⚑1 ◐3` (one blocked, one approval, three working) and
the `docs` tab carries `⚑`. Pane headers show role tint, state, member and task. The `needs you` lane is first in
the work board even though `cy` is the third member of its role.

### 5.2 Full screen, 120 columns

```text
 SPACES                       │ main ▪▪▫ ◐2   tests ◐   docs   +                                            ▲1 ⚑1 ◐3 ○1
 ▸ aster                    ◐ │┌▌builders ◐ ada · fix flaky auth test ─12m ┐┌▌builders ▲ cy · migrate session ────⚑ 4m ┐
   api-gateway              ▲ ││$ cargo nextest run                        ││Allow Bash(rm -rf sessions/old) ?         │
   docs                     ○ ││  Running 214 tests                        ││                                          │
──────────────────────────────││  PASS auth::refresh                       ││  ❯ 1. Yes                                │
 TEAM                      ▲1 ││  FAIL auth::token_race                    ││    2. No                                 │
 ◐ Polaris               lead ││                                           ││    3. Yes, and don't ask again           │
 ▌ builders             ◐1 ▲1 ││> fixing the race…                         ││                                          │
 ▌ ◐ ada          claude·opus ││                                           ││                                          │
 ▌ ○ bo         codex·gpt-5.1 ││                                           ││                                          │
 ▌ ▲ cy        gemini·2.5-pro ││                                           ││                                          │
 ▌ reviewers               ○1 ││                                           ││                                          │
 ▌ ○ rae        claude·sonnet ││                                           ││                                          │
                              ││                                           ││                                          │
 WORK                1⚑ 2◐ 1○ ││                                           ││                                          │
 ▲ needs you  1               ││                                           ││                                          │
   ⚑ builders              4m ││                                           ││                                          │
     migrate session store    ││                                           ││                                          │
 ◐ working  2                 ││                                           ││                                          │
   ◐ builders  ━━─        12m ││                                           ││                                          │
     fix flaky auth test      ││                                           ││                                          │
     ●━◐━○  step 2/3 → codex  ││                                           ││                                          │
   ◐ reviewers ━──         3m ││                                           ││                                          │
     review parallel checkout ││                                           ││                                          │
     ‖ parallel 1/2  ◐ ✓      ││                                           ││                                          │
 ○ queued  1                  ││                                           ││                                          │
   ○ docs                  1m ││                                           ││                                          │
     update the cli docs      ││                                           ││                                          │
 ✓ done  1                    ││                                           ││                                          │
   ✓ builders              3m ││                                           ││                                          │
```

At 120 columns the pane header drops the model (see tiers below) and the sidebar shows `kind·model` and the next
relay step (`→ codex`).

### 5.3 Sidebar anatomy

```text
 TEAM                 ▲1     L1 label + attention count (only when > 0)
 ◐ Polaris          lead     lead: state glyph, name (bold), "lead"
 ▌ builders        ◐1 ▲1     role card header: tint gutter, name, per-state counts
 ▌ ◐ ada     claude·opus     member: gutter, state, name, kind·model (right aligned)
 ▌ ○ bo      codex·gpt-5     idle members are neutral
 ▌ ▲ cy           gemini     model dropped first when the row is tight
 ▌ reviewers          ○1     one-member roles still get a card
```

Rules: counts show only non-zero states, ordered blocked, approval, working, queued, idle; a **collapsed** role
shows the same counts plus a trailing `▸`; `pinned` roles keep the word `pinned` at wide tier. A member without
a live agent shows its supervision phase instead of the kind (`relaunching`, `gave up`) in `muted`, dimmed, as
today. The lead row is clickable like today; every member row keeps its existing hit target.

### 5.4 Work board as lanes

```text
 WORK           1⚑ 2◐ 1○      L1 label + summary chips
 ▲ needs you  1               lane header: glyph + name + count, collapsible
   ⚑ builders         4m      task card row 1: glyph, role (who), [bar], age
     migrate session…         row 2: title (L5)
 ◐ working  2
   ◐ builders ━━─    12m      age bar: 3 cells, log-scale buckets (below)
     fix flaky auth…
     ●━◐━○  step 2/3          row 3 for a relay: nodes, "step N/M"
   ◐ reviewers ━──    3m
     review checkout…
     ‖ parallel 1/2           row 3 for parallel: count; wide tier adds branch glyphs ◐ ✓
 ○ queued  1
 ✓ done  1                    resolved, newest first, 10 min window
```

- **Age bar** (3 cells, `━` filled / `─` empty): fills by log-scale buckets of elapsed time, `< 1m`, `< 5m`,
  `< 15m`, `< 1h`, `>= 1h` mapped to 0,1,2,3 cells and a color step at 15 min (`muted` → `state_approval`) and 1 h
  (→ `state_blocked`) **only for the `working` and `queued` lanes**. It is a gentle "this has been a while" signal,
  not an alarm. Needs-you tasks show the bar in `state_approval`, because waiting is the signal. Computed per frame from `created_at_ms` with
  integer compares; no allocation.
- **Relay strip** (row 3): up to 8 nodes from `steps` (`●` done, spinner/`◐` active, `○` pending, `✗` failed)
  joined by `━` (the strip window is cosmetic; if more than 8 steps exist the nodes elide the middle with `…`), then `step N/M` from `FlowSummary`; at wide tier ` → codex` names the next member's kind. Without `steps` data
  (older server) the row is omitted and nothing else changes.
- **Parallel**: `‖ parallel 1/2` counts finished steps over **all** parallel steps (`OrbitShellParallel`, not the visible window); wide tier appends one state glyph per branch.
  `integrating` replaces it with `⊕ integrating` while the integrate step is active.
- **Approval**: `⚑` (peach) leads the card row, replaces the state glyph, and puts the card in `needs you`
  even when the worker state says `working`.
- Lane headers toggle collapse by click, through a new hit-target type (section 6.6). Lane collapse state is a
  separate `collapsed_lanes: LaneSet` (a `u8` bit set of `Lane`), not strings in `collapsed_team_groups`, so a lane
  can never collide with a role named `needs_you`.
- Task rows still focus the worker's pane on click; recent rows stay non-interactive as today.

### 5.5 Tab bar

```text
 main ▪▪ ◐2   tests ◐   docs ⚑   +           ▲1 ⚑1 ◐3
 main ▪▪▫ ◐2   tests ◐   docs ⚑   release   +                                ▲1 ⚑1 ◐3 ○1
```

- Per tab: `name` + `▪▫` role chips (one per role with a member in the tab; tinted; `▪` filled while that role has a working member, `▫` otherwise; max 4) + `◐N` working count + `▲`/`⚑` when anything in the tab is blocked/awaiting approval. The focused tab keeps the accent pill (accessibility: a fill, not only a color).
- The existing `tab_bar_right` segments stay; the **attention chip** `▲1 ⚑1 ◐3` sits left of them when the strip has room (`MIN_TAB_STRIP_WIDTH` already guards), omitted at zero counts.
- The one-shot tab sweep (`tab_sweep`) stays as is. The spinner stays in column 0 of a tab that has working agents.
- All of it is read from a per-snapshot `TabActivity` cache (also fixes P16 for modern).

### 5.6 Pane headers (the top border row)

A slim worker header is painted on the pane's top border row, from the same precomputed task/member data.
Drop order by inner header width, rightmost first (model, then age, then kind, then role name, then title):

```text
wide   (>=64 inner)
┌▌builders ◐ ada claude·opus-5.5 · fix flaky auth test ───── ━━─ 12m ┐
medium (48-63)
┌▌builders ◐ ada claude · fix flaky auth test ──── 12m ┐
compact (32-47)
┌▌builders ◐ ada · fix flaky auth test  12m ┐
narrow (<32)
┌▌ada ◐ fix flaky auth… ──┐
blocked
┌▌builders ▲ cy gemini · migrate session store ── ⚑ 4m ┐
idle member, no task
┌▌reviewers ○ rae claude·sonnet ───────────┐
```

- The role tint is the `▌` at the left, the state glyph follows, then member, `kind·model`, `·`, task title, then right-aligned age (and `⚑` when awaiting approval).
- **Focused**: border and header in `focus`, header bold. **Unfocused**: `muted`. **Recently changed**: the state glyph flashes and the header text blends toward `text` for the recency window (existing `recency_highlight`). **Blocked**: the `▲` pulses (full only); the border color does not change.
- Panes that are not Orbit workers keep the classic title (agent label if enabled).
- Panes without a top border (single pane, or `pane_borders = "never"`) show no header; the sidebar and the tab bar carry the information. Modern never adds a row, so it never resizes a pane.

### 5.7 Flow view (phase 4)

A centered overlay built with the existing `panel`/`popup` helpers and the overlay input path
(`overlay_input.rs`): `Esc` closes, `↑/↓` moves between steps, `←/→` switches task, `Enter` focuses the selected
step's pane, mouse click does the same. 80 columns, vertical:

```text
┌─ flow · fix flaky auth test ────────────────────────── 9c1e · 12m · esc ─┐
│ Polaris ━▶ builders                                      ◐ working 2/4   │
│                                                                          │
│ ✓ ada   claude·opus-5.5    4m   reproduced: refresh races the token      │
│ │                               store; failing test added                │
│ ╰▶ "patch the refresh path; keep all 214 tests green"                    │
│ ◐ bo    codex·gpt-5.1      2m   ━━━━──────                               │
│ │                                                                        │
│ ┬ parallel 0/2                                                           │
│ ├ ○ rae   claude·sonnet    waiting   tests/auth/                         │
│ └ ○ cy    gemini·2.5-pro   waiting   docs/                               │
│ │                                                                        │
│ ⊕ integrate      ○ waiting      Polaris merges 2 patches                 │
│                                                                          │
│ tasks ▸ [fix flaky auth test ◐]  migrate session ⚑  review checkout ◐    │
└──────────────────────────────────────────────────────────────────────────┘
```

At 120 columns parallel steps sit side by side, then the integrator:

```text
┌─ flow · fix flaky auth test ────────────────────────────────────────────────────────────── 9c1e · 12m · esc ─┐
│ Polaris ━▶ builders                                                           ◐ working 2/4                  │
│                                                                                                              │
│ ✓ ada   claude·opus-5.5    4m   reproduced: refresh races the token store; failing test added                │
│ │                                                                                                            │
│ ╰▶ "patch the refresh path; keep all 214 tests green"                                                        │
│ ◐ bo    codex·gpt-5.1      2m   ━━━━──────                                                                   │
│ │                                                                                                            │
│ ┬ parallel 1/2                                                                                               │
│ ├────────────────────────────────────────────────────┬────────────────────────────────────────────────────┐  │
│ │ ◐ rae  claude·sonnet   tests/auth/                 │ ✓ cy  gemini·2.5-pro   docs/  patch ready          │  │
│ │   ━━━━━━━━────  3m                                 │   4 files +62 −9                                   │  │
│ └────────────────────────────────────────────────────┴────────────────────────────────────────────────────┘  │
│ │                                                                                                            │
│ ⊕ integrate      ○ waiting      Polaris merges 2 patches into builders' checkout                             │
│                                                                                                              │
│ tasks ▸ [fix flaky auth test ◐]  migrate session ⚑  review parallel checkout ◐                               │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- Source: `OrbitShellActiveTask.steps` (+ `step_total`). Handoff notes are the redacted first line of `done`
  (and `next` is shown on the arrow row). Without `steps` the overlay shows one node (lead → worker).
- **Completion and identity stay visible under truncation.** The header always shows `parallel 3/4 done` and
  `step 2/5` from `FlowSummary` (all steps), never from the rendered rows. Every step row keeps
  `member·kind` as its leftmost, last-to-drop field; when a step has flow focus its row is expanded with the
  same identity plus its note, and when width is short the **note** is truncated first, then the age, then the
  model, member/kind are retained before other fields. At widths too small to fit that identity, clip by display width within the row; never overflow or assume arbitrary identities fit. Steps outside the note window render as identity-only rows (state glyph,
  member, kind); steps beyond the identity list collapse into one `… N more before/after` row that is not
  focusable but whose count comes from `steps_omitted`. Parallel branches show a branch counter (`3/4`) on the
  `┬ parallel` row even when only some branches fit on screen at 80 columns.
- Entry points: `keys.orbit_flow`, the global menu, the work header (modified click or an explicit `flow` link at wide tier),
  and the task row context menu. All absent under `ui.style = "classic"`; the key is inert and not listed in help there.
- `ui.orbit_flow = false` removes the entry points while staying modern.

### 5.8 Toasts and the mode bar

```text
classic   ┌───────────────────────┐        modern     ▌ ▲ cy needs approval
          │ ● cy blocked          │                   ▌   Bash(rm -rf sessions/old)
          │   Bash(rm -rf sess…   │
          └───────────────────────┘
```

- Modern toast: no box, a 1-cell left bar in the event's state color, glyph by event kind (`▲` blocked, `⚑`
  approval, `✓` finished, `✗` failed, `◐` started), title in `text`, body in `muted`. Same slide/fade rules as today;
  width computed with `display_width` (fixes P15 for modern).
- Mode bar: the chip style from the summary (`NAV` instead of `NAVIGATE` below 60 columns; the mode
  name keeps its fill), key names in `focus`, hints in `muted`, and the attention chip right-aligned. It still only
  draws outside Terminal mode, so the always-visible summary lives in the tab bar (P13).

```text
 NAVIGATE  esc back  ↑/↓ workspace  tab pane  ? keybin
 NAV  esc back  ↑↓ space  tab pane  ? keys   ▲1 ⚑1 ◐3
```

### 5.9 Collapsed sidebar and mobile

- **Collapsed sidebar** (`sidebar.rs:47`): unchanged geometry; modern uses the new state glyphs and colors, a role
  tint in the glyph column's left neighbour when there is one free cell, and shows `▲`/`⚑` for any workspace that
  needs attention (same attention rule as lanes).
- **Mobile** (below 64 columns): one header row gets the attention chip; the existing `mobile.rs` items adopt the
  glyph set and roles. No panes, no sidebar, no flow side-by-side; the flow overlay stacks vertically as at 80.

```text
 ▲1 ⚑1 ◐3   main                                   [ menu ]
────────────────────────────────────────────────────────────
   (the active pane fills the screen)
```

### 5.10 `aster orbit activity`

Same lanes, glyphs and age units as the TUI. It prints plain glyph text when stdout is not a TTY
(`NO_COLOR`/pipes already switch off color) and uses `ui.glyphs`. Under `ui.style = "classic"` the output is byte-identical to today.

```text
Orbit  ·  3 workers  ·  ▲ 1 needs you   ◐ 2 working   ○ 1 queued

▲ NEEDS YOU
  ⚑ 7f3a  builders/cy     4m  migrate session store
        waiting for approval: Bash(rm -rf sessions/old)
◐ WORKING
  ◐ 9c1e  builders/bo    12m  fix flaky auth test         ●━◐━○ step 2/3
  ◐ 3b20  reviewers/rae   3m  review parallel checkout    ‖ parallel 1/2
○ QUEUED
  ○ 51aa  builders        1m  update the cli docs
✓ DONE
  ✓ 44ed  builders/ada    3m  bump deps, tests pass
```

### 5.11 Settings

`ui.style` and `ui.glyphs` appear in the existing settings overlay (`settings.rs`; sections today are Theme,
Indicators, Sound, Toast, Integrations, Polaris) as rows in the `Indicators` section, using the same
select-one control as the status indicator style. No new screen. (`ui.animations` is not in the overlay today and
stays out of scope.)

## 6. Architecture

### 6.1 The style switch

```
ui.style (config) ──► ClientShellConfig.chrome: Chrome { style, glyphs, roles }   (built once; rebuilt on reload / theme change)
                           │
   render entry points ────┴──► match chrome.style { Classic => existing fn (untouched), Modern => modern::draw::… }
```

`style` is read **only at the dispatch points in the table below**, one per surface. Each is a single `match`
(or an early `if chrome.style == Classic { return existing(..) }`) whose classic arm calls the existing function with
its existing arguments. Everything modern is in new files. Classic function bodies are not edited (section 9).
Shared pure helpers (`age_text`, `put_number`, `blend_color`) are moved/re-exported, not changed. Style is
client-local configuration (`ClientShellConfig::from_config`); a remote or cloud endpoint is drawn with the
local client's style.

| Surface | Dispatch point (classic arm = existing fn) | Phase |
|---|---|---|
| Expanded sidebar, team tree and work board | `render_team_panel` / `render_team_line` (`team_sidebar.rs`) | 2 |
| Expanded sidebar, flat agent list and workspace rows | glyph/color indirection only: `agent_status_icon_and_color`, `status_icon`, `status_color` take `&Chrome` and return the classic values unchanged when classic; row layout code is not dispatched | 2 |
| Collapsed sidebar | top of `render_collapsed_sidebar` (`sidebar.rs:47`): `if classic { existing body }`; its entry list `collapsed_entries` is shared | 2 |
| Multi-endpoint sidebar (`endpoint_sidebar.rs`) | classic until phase 6; then glyph/color indirection only | 6 |
| Tab bar | top of `render_tab_bar` (`tabs.rs`); `tab_bar_status_width` unchanged | 3 |
| Pane headers | the header pass in `composition.rs`: `if classic { skip }` (classic draws nothing there; the server-drawn border row stays as is) | 3 |
| Mobile layout | `render_mobile_header` and the `MobileItem` builders (`mobile.rs`): classic arm is the current body; modern adds the attention chip and glyph/role indirection only | 3 |
| Mode bar | top of `render_mode_bar` (`render.rs:31`) | 5 |
| Toasts: card, mobile banners, copy feedback, config diagnostic | `render_visible_notification[_with_phase]`, `render_mobile_notification_banner[_with_phase]`, `render_notification_card_with_phase` (`notifications.rs`); `status.rs` functions are not dispatched (P15 is a separate fix) | 5 |
| Overlays | opener for `OrbitFlow` (absent when classic); `help_lines`, global menu and settings rows add entries only when modern; every other overlay is not dispatched | 1, 4 |
| `aster orbit activity` | entry of the render function in `cli/orbit_activity.rs`: `match style`; classic arm is today's function. Style/glyphs come from `Config::load()` (the same loader other CLI commands use); an unreadable config yields the defaults, as everywhere else. Machine-readable output (JSON) is never styled | 5 |
| Window title, OS notifications, sound, pane surface content | not style-aware | none |

`chrome` is passed to the classic helpers that need glyph/color indirection through `ClientShellConfig` (already
passed to every renderer), with minimal signature plumbing where the current helper does not receive config. Preserve the classic helper result and characterize it; do not assume every existing helper already accepts `ClientShellConfig`. Two guards keep the table honest:

- the architecture script (section 6.5) gains a check that `modern::` (and `ui::chrome::` role/glyph
  lookups) are referenced **only** from the files in this table, to flag unlisted direct references; definition/config wiring needs an explicit allowlist and indirect aliases still require review; and
- each row has a golden test that renders the surface with `style = classic` and compares it with the phase-0
  fixture (section 9), including mobile (width <= 64), the collapsed sidebar, and the CLI output captured with a
  fixed activity payload.

### 6.2 Module layout

```
src/ui/chrome/                    pure, no AppState, no ClientShell types (usable by the CLI too)
  mod.rs
  glyphs.rs                       GlyphSet (UNICODE, ASCII) consts, spinner sets
  roles.rs                        ChromeRoles::derive(&Palette), ensure_contrast, role_tint(index)
  state.rs                        StateKind, Lane, wire-string → enum maps
  age.rs                          age_text (moved from work_board.rs), age bar buckets
  draw.rs                         allocation-free cell writers: put_str, put_u16, put_age, put_bar, put_right
src/client/shell/modern/
  mod.rs                          Chrome, dispatch helpers
  model/                          MAY allocate. Built on snapshot / config change only.
    team.rs                       ModernTeam, RoleCard, MemberRow (preformatted "claude·opus")
    lanes.rs                      Lanes, TaskCard, StepStrip
    tabs.rs                       TabActivity, AttentionSummary
    pane_header.rs                PaneHeaders: pane id → PaneHeader
    flow.rs                       FlowModel (built on overlay open / snapshot change while open)
  draw/                           MUST NOT allocate. Reads models, writes cells.
    sidebar.rs  lanes.rs  tabs.rs  pane_header.rs  flow.rs  toast.rs  status.rs
```

New server data builders sit in `src/app/orbit_notifications/shell_view.rs` (existing file; ~60 added lines)
or a sibling `shell_view_steps.rs` if it would pass 400 lines.

### 6.3 Where precomputation happens (render stays pure)

| Derived thing | Built when | Stored in | Read by |
|---|---|---|---|
| `ChromeRoles`, `GlyphSet` | config build, reload, theme change | `ClientShellConfig.chrome` | all modern draw |
| `ModernTeam`, `Lanes` | `orbit` projection changes (`apply_active_snapshot`, same place as `TeamSidebar::build`) | `TeamSidebar` (new field) | team draw |
| cached row list (lines, heights, gaps) | `refresh_team_rows` (key now includes `style` and `glyphs`) | `TeamRows` | team draw |
| `TabActivity`, `AttentionSummary` | `set_snapshot` (once per `snapshot_serial`) | `ClientShellState` (`chrome_cache`) | tab bar, mode bar, mobile header |
| `PaneHeaders` | `set_snapshot`, keyed by pane id | `chrome_cache` | pane-header pass |
| `FlowModel` | overlay open, snapshot change while open, resize | the overlay value | flow draw |
| age text, age bar, spinner/pulse phase | **per frame**, stack buffers and integer compares | nowhere | draw |

#### Cache keys and invalidation

Rule: **every precomputed value that depends on width, height, theme, style, glyphs, collapse state or the
snapshot lives in a struct with an explicit key holding exactly those inputs.** The owning `refresh_*` compares
the frame's inputs with the key before the frame is drawn and rebuilds on mismatch; draw only reads. Values
that are plain arithmetic clamps of a rect (truncation to the width given, drop-order tiers, alignment) are
**not** cached: they are integer compares in draw. Nothing is keyed on the animation tick or on the clock except
the done-lane expiry deadline.

| Precomputed value | Depends on | Key (all compared in `refresh_*`) | Rebuilt when |
|---|---|---|---|
| `ChromeRoles`, `GlyphSet` | palette, `ui.style`, `ui.glyphs` | the three inputs | config reload, theme change |
| `ModernTeam`, `Lanes`, `FlowSummary` | `orbit` facts | `orbit` projection (`same_facts`) | orbit projection change |
| `TeamRows` (lines, heights, gaps, lane truncation `+N`, spacer rows) | snapshot, collapsed groups, **collapsed lanes**, sidebar content width, **sidebar section height**, density tier (derived from width), sort, `sidebar_work`, agents config, `style`, `glyphs`, done-lane expiry | existing `TeamRowsKey` plus `collapsed_lanes`, `section_height`, `style`, `glyphs` | any key field differs or `now_ms >= expires_at_ms` |
| `TabActivity`, `AttentionSummary`, `PaneHeaders` (full, untiered strings) | snapshot, style, glyphs, roles | `snapshot_serial`, `style`, `glyphs`, palette/roles revision | those change; **not** width, because header tiers and chip truncation are draw-time arithmetic over the rect |
| Collapsed sidebar entries | snapshot, sort | `snapshot_serial`, sort | those change; row count clipped to the area at draw |
| Mobile header strings | snapshot, width tier | `snapshot_serial`, actual width tier, style, glyphs, palette revision | those change |
| `FlowModel` rows (wrapped notes, column split) | active task, selected step, overlay inner width **and height**, glyphs | `(endpoint_id, task_id, selected_step_index, snapshot_serial, inner_width, inner_height, style, glyphs)` held in the overlay | open, snapshot change while open, **terminal resize**, task switch, selected-step change |
| Notification card text and glyph | event | built once at creation (`notification_policy.rs`) | never; card geometry is arithmetic against the area each frame |
| CLI table | one activity payload | none (one-shot process) | n/a; terminal width is read once at start |

Classic must not start rebuilding more often than today: `section_height` and the modern key fields are stored as
`0` / constants when `style = classic`, so the classic key compares equal exactly as it does now. Height arrives
at `refresh_team_rows` from the layout already computed for the frame (`expanded_sidebar_sections`), so the
check needs no extra layout pass. Tests (phase 2 onward) resize width and height across every tier boundary
(`<22`, `22..31`, `>=32` wide; `height` 29/30 for spacers; overflow that adds or removes `+N`) and assert that
the rows rebuild exactly once per change and zero times between identical frames (`row_builds()` counter).

`compose()` already takes `&mut self` and fills `hits`; the header pass only reads models and the animation
frame and writes cells. Time-dependent facts are limited to ages, the age bar buckets and the done-lane expiry,
which `TeamRows::expires_at_ms` already handles.

### 6.4 Pane headers are client-painted

The server draws border rows into the pane surface (`render_pane_borders` / `render_pane_border_titles`);
the client blits that surface, then knows every pane's `rect`/`inner_rect` (`PaneSurfacePane`) and has
`snapshot.orbit` and `snapshot.agents`. The header pass runs right after the pane blit in `composition.rs`:

- Only for panes with a top border (`inner_rect.y > rect.y`), not popups, and `rect.width >= 6`.
- It writes into the span between the corner cells and **stops at, and never overwrites, junction or corner
  cells** (`┬ ┴ ┼ ├ ┤ ┌ ┐ └ ┘ │`), so shared dividers stay intact. Any server-drawn label text in the span is
  replaced.
- Per pane cost: one hash lookup by borrowed `&str` pane id, O(header width) cell writes.
- Why not server-side: it keeps presentation state in the client (runtime/client boundary), needs no wire
  change, works unchanged for remote and cloud endpoints, and does not invalidate pane surfaces when Orbit
  facts change (the surface cache keys on pane content and topology).

### 6.5 Enforcing pure render

`scripts/test_ui_hot_path_architecture.py` is extended in phase 1:

- `HOT_PATH_SOURCES` already recursively covers `src/ui/**`; add `src/client/shell/modern/draw/**`. Apply the new allocation rules only to `src/ui/chrome/draw.rs`, `src/ui/chrome/age.rs`, and `src/client/shell/modern/draw/**`, not configuration-time role derivation or model builders.
- A new check forbids, outside `#[cfg(test)]`, in those draw/age sources: `format!(`, `.to_string()`, `.to_owned()`,
  `.clone()`, `vec!`, `String::`, `Vec::`, `.collect`, `Box::new`, `HashMap`/`HashSet` construction, `std::fs`, `Instant::now`, `SystemTime`.
- A dispatch-whitelist check: `modern::` paths and `ui::chrome` role/glyph lookups may appear only in the files named in the section 6.1 table (plus `modern/**` itself), as a direct-reference regression guard. Explicitly allow definition/wiring files (`chrome/**`, shell module/config/state, CLI wiring) and test modules; helper age imports are not role/glyph dispatch. Test aliases, grouped imports, whitespace and `crate::`/`super::` forms. A lexical whitelist cannot prove surface coverage or detect arbitrary re-exported aliases; pair it with review and surface goldens.
- Its scanner reuses the existing comment/literal masking, with unit tests like the existing
  `test_scanner_*` ones. `src/client/shell/modern/model/**` is deliberately outside it.

This makes the directory the rule: if code needs to allocate it cannot live in `draw/`.

### 6.6 Hit targets for new clickable elements

Today `ShellHitMap` (`state.rs`) has `team_groups: Vec<(Rect, usize)>` (the `usize` is a group index where
`groups.len()` means the `other` group and `groups.len() + 1` the work header, see `group_key`) and
`team_agents: Vec<(Rect, usize)>` (a cached line index resolved by `line_pane_id`). A lane header cannot be
expressed as a group index without changing what that integer means, so it gets its own type instead:

```rust
pub(super) enum ChromeTarget {
    Lane(Lane),                          // toggle collapse of a work lane
    Attention,                           // attention chip: focus the oldest needs-you pane
    FlowOpen { task: u16 },              // work header / task row "flow" link
    FlowTask(u16),                       // flow overlay: task chip
    FlowStep { task: u16, step: u16 },   // flow overlay: step row, focuses its worker pane
}
// Task ordinals resolve through a retained snapshot/task-id table; stale-generation events are discarded.
// ShellHitMap { .. , chrome_targets: Vec<(Rect, ChromeTarget)> }   // empty unless modern draws
```

- Layout/update prepares the bounded `chrome_targets` storage and its capacity before drawing. Draw writes only into prepared slots (or an explicitly capacity-checked reusable vector, with no growth); it must never allocate through `push`. Modern records into `hits.chrome_targets`; classic never does, so the vector is empty and the existing
  `mouse.rs` branches (`team_groups` at the left-click handler, then `team_agents`, then `agents`) behave exactly as
  today. The modern lookup is one extra `find` placed **after** the endpoint-agent and `team_groups` checks and
  before `team_agents`, guarded by `!chrome_targets.is_empty()`.
- Role headers and the `other` header keep pushing `team_groups` with the same group index; member, lead and task
  rows keep pushing `team_agents` with the line index. The classic work header (group index `groups.len() + 1`) is
  only produced by the classic draw. In modern the work header is a label with no click target; its lanes are
  `ChromeTarget::Lane`.
- The flow overlay keeps its own hit rectangles inside the overlay value (as `settings_tabs` and the other overlays
  do) and uses `FlowTask`/`FlowStep` there; `FlowOpen` is the only flow target on the shell. Tab role chips and pane
  headers add no targets: they sit inside the existing tab and pane-border rectangles. The toast and the mode bar
  keep their current targets.
- Every place that clears hits when mouse capture is off (`render.rs`, the `!config.mouse_capture` block, and
  `state.rs` where `team_groups` is cleared) also clears `chrome_targets`.
- Keyboard: lane headers are not focus targets (neither are group headers today). Selection and reveal keep using
  the cached `rows.lines` order (P19), so a lane header or `Gap` line can never desynchronize them.
- Tests: for each target kind, a rect at every density tier resolves to the right target at its first and last
  cell and to nothing one cell outside; classic produces no `chrome_targets`; the existing group/agent hit tests
  pass unedited.

## 7. Data contract: additive fields only

All additions are on `OrbitShellView`'s existing structs in `src/protocol/wire.rs`, optional, with
`#[serde(default, skip_serializing_if = ...)]` exactly like `server_now_ms`. The frozen fixtures
(`tests/fixtures/endpoint-snapshot-v1.json`, `endpoint-method-shapes-v1.json`) deserialize unchanged because
every new field defaults and is omitted when empty; no existing field, enum variant, method or codec changes. The
snapshot is a JSON control, not a bincode codec.

```rust
pub struct OrbitShellMember {
    // existing: name, kind, terminal_id, pane_id, state
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,                 // roster member's model; absent for scratch workers
}

pub struct OrbitShellActiveTask {
    // existing: task_id, worker_terminal_id, role, title, state, created_at_ms, blocked, awaiting_approval
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<OrbitShellStep>,            // identity for at most 32 steps; notes only inside the 8-step window; empty for an unrelayed task
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_total: Option<u16>,               // pipeline length, as orbit_step_line computes it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<OrbitShellParallel>,  // totals over ALL parallel steps, not the window
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps_omitted: Option<OrbitShellOmitted>, // steps beyond the 32-step identity list
}

pub struct OrbitShellParallel { pub done: u16, pub total: u16 }
pub struct OrbitShellOmitted { pub before: u16, pub after: u16 }

pub struct OrbitShellStep {
    pub index: u16,
    pub member: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub worker_terminal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub model: Option<String>,
    pub state: String,        // pending|active|done|handed_off|failed; opaque strings tolerated
    #[serde(default, skip_serializing_if = "Option::is_none")] pub note: Option<String>,    // handoff `done`, first line, redacted, <= 80 chars
    #[serde(default, skip_serializing_if = "Option::is_none")] pub next: Option<String>,    // handoff `next`, same treatment
    #[serde(default, skip_serializing_if = "Option::is_none")] pub parallel: Option<String>,// waiting|launching|starting|running|capturing
    #[serde(default, skip_serializing_if = "Option::is_none")] pub integrate: Option<String>,// integrate phase, opaque
    #[serde(default, skip_serializing_if = "Option::is_none")] pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub finished_at_ms: Option<u64>,
}
```

- `OrbitShellView::same_facts` destructures every field exhaustively, so the compiler forces the new fields
  into the equality that decides whether a snapshot is re-sent. Nested member/task equality derives include the added fields automatically; the exhaustive top-level destructure alone does not enforce nested comparison. Struct literals in `shell_view.rs`, `client/shell/tests/team_sidebar.rs` and `client/shell/tests/work_board.rs` need explicit new fields. Keep `Eq` on `OrbitShellStep`; test mutations of each added fact.
- **Identity is bounded; detail is windowed.** Two limits, applied when the view is projected, never while drawing:
  - `steps` carries **identity for every step up to 32** (`index`, `member`, `kind`, `state`, `parallel`, `integrate`). `step_total` is the full count (checked/saturating conversion to `u16`). Use a contiguous window of at most 32 original indices containing the active/fan-out/integrate set, then nearest neighbors; clamp the window at pipeline ends. Beyond that window, excess is reported through `step_total` and omitted before/after counts. Never take the first 32 unconditionally. If the required set cannot fit contiguously, retain active steps first and explicitly mark incomplete identity coverage; do not fabricate complete flow data.
  - Heavy text (`note`, `next`, timestamps) is filled only for a **window of at most 8 steps** that prioritizes: every active step; every step of the current parallel fan-out and the integrate step (a fan-out is at most `MAX_PARALLEL = 4`); then the nearest finished/pending steps by index until 8. If the required set exceeds eight, retain active steps first, then the current fan-out/integrator, and leave the rest identity-only; the cap is unconditional. Original `index` values are preserved, so gaps are visible.
  - Two summaries make completion independent of the window. `OrbitShellActiveTask.parallel: Option<OrbitShellParallel { done: u16, total: u16 }>` counts **all** parallel steps of the task, in or out of the window (`done` = steps with `done`/`handed_off`; `total` includes failed). `steps_omitted: Option<OrbitShellOmitted { before: u16, after: u16 }>` counts steps beyond the 32-step identity list. A first-N cap is therefore never the source of any total.
- The client builds `FlowSummary` with the lanes (at projection change): `parallel_done`, `parallel_total`, `relay_current_index`, `relay_total`, `omitted_before/after`, and one preformatted `member·kind` string per step. The sidebar strip (`parallel 3/4`, `step 2/5`) and the flow header read these, never a count of the visible window. Relay `step N/M` uses the retained active step's original index and `step_total`; it does not claim a total completed count, which omitted identities cannot establish. If no active index is available, show the total without inventing N.
- **Privacy.** `note`/`next` are worker-authored text. They go through the existing `display_text`
  (`redact_secrets` then `shorten`, `shell_view.rs`) exactly like `title` and `summary`, are capped at 8 windowed steps x 2
  strings per task, and exist only for active tasks (at most `MAX_ACTIVE_TASKS = 32`). Handoff `changed_files`, `known_issues` and full prompts are **not** projected.
- **Skew.** New client + old server: missing fields default; model and step rows are omitted, the rest renders. Old client + new server: unknown JSON fields are ignored (no `deny_unknown_fields`). A new
  `state` string is opaque (`StateKind::Unknown`).
- No new API method or capability is required, so there is nothing to advertise and the generation-1 contract is untouched.
- Everything else (lanes, counts, attention summary, role tints, age buckets, pane headers) is derived client-side from existing fields.

## 8. Phase plan

Each phase ends with: `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
`cargo nextest run --locked`, `python3 -m unittest scripts.test_ui_hot_path_architecture`,
`python3 scripts/config_reference_check.py`, `just docs-contract-test`, and `just bench-render-scale` before
and after (1 and 15+ panes, plus the Orbit scenario from phase 2). Classic golden tests must pass unedited.

### Phase 1: visual foundation

Build: config, roles, glyphs, drawing helpers, enforcement. **No visible change** (modern equals classic until later phases switch pieces on).

- `src/config/model.rs`: `UiStyleConfig { Modern (default), Classic }`, `UiGlyphsConfig { Unicode (default), Ascii }`, fields `ui.style`, `ui.glyphs`; parse and reject-invalid tests like `ui.animations`.
- `src/client/shell/{config.rs,state.rs}`: `ClientShellConfig.chrome`, built in `from_config`, refreshed in the reload path next to `self.animations = ui.animations` (`config.rs:365`) and on theme change.
- New `src/ui/chrome/**` (section 6.2). Move `age_text`/`put_number` here; `work_board.rs` imports them (output byte-identical).
- `src/ui/sidebar.rs`: add the ASCII spinner set beside `WORKING_SPINNER_GLYPHS`.
- Docs: `docs/next/website/src/content/docs/configuration.mdx` (+ `ja/`, `zh-cn/` mirrors, same heading shape; `scripts/test_docs_translation_parity.py` compares heading outlines) and `docs/next/website/src/data/config-reference.json` rows for `ui.style`, `ui.glyphs`. `python3 scripts/config_reference_check.py` must pass.
- `scripts/test_ui_hot_path_architecture.py`: section 6.5.
- Settings overlay rows for `ui.style` and `ui.glyphs` in the `Indicators` section (`settings.rs`).
- Tests: classic golden fixtures (section 9) **generated first, from this phase-0 commit**; contrast test over all 18 themes; glyph width test (every glyph is width 1, ASCII set is ASCII); role-derivation tests (light/dark/terminal); config parse tests.

### Phase 2: sidebar (team tree as role cards, lanes)

- Wire/server: `model`, `steps`, `step_total`, `parallel`, `steps_omitted` (section 7) in `wire.rs` and `app/orbit_notifications/shell_view.rs`; tests: JSON round-trip, old payload decodes, `same_facts` sensitivity, redaction of notes, the 32-step identity cap and 8-step note window (active/fan-out/integrate priority with the unconditional eight-detail cap), `parallel`/`steps_omitted` totals correct when the window is truncated.
- Client models `modern/model/{team,lanes}.rs` built in `TeamSidebar::build` (so one build per orbit projection change). `TeamRowsKey` gains `style`, `glyphs`, `collapsed_lanes` and `section_height` (see "Cache keys and invalidation" in section 6.3; `refresh_team_rows` receives the section height); `TeamLineRef` gains `LaneHeader { lane, collapsed }` and `Gap` (blank row, tall layouts only).
- Draw: `modern/draw/{sidebar,lanes}.rs`, dispatched from `render_team_line` in `team_sidebar.rs`. Role and `other` headers keep `hits.team_groups` (`usize` group index) and task/member rows keep
  `hits.team_agents` (line index), so their classic handling in `mouse.rs` is untouched. Lane headers and every
  other new clickable element use the new `hits.chrome_targets` (section 6.6); lane collapse state is `collapsed_lanes`.
- **Fix P19 first, in classic too and behavior-preserving:** make `row_index_of` read the cached `rows.lines` instead of recomputing order, guarded by a test that compares old and new results over many states.
- Collapsed sidebar: dispatch at the top of `render_collapsed_sidebar` (section 6.1 table); modern takes the glyph/role set and the attention marker.
- `ShellHitMap.chrome_targets` and `ChromeTarget::Lane` (section 6.6), `collapsed_lanes` state, and the `mouse.rs` lookup.
- Bench: add the Orbit-populated scenario (R roles x M members, T tasks, 1 and 15+ panes) to `render_scale_benchmark.rs`; record **classic** numbers before modern lands.
- Tests: buffer snapshots at 80 and 120 and widths 18/25/35 for light/dark/terminal themes; animations full/reduced/off; keyboard navigation parity (`pane_order` unchanged); mouse hit rectangles for every row kind; collapsed groups and lanes; 32 active tasks; missing `steps`/`model` (old server); a member without a live agent.

### Phase 3: tab bar, pane headers, per-role activity

- `modern/model/{tabs,pane_header}.rs` built in `set_snapshot`; stored in `ClientShellState.chrome_cache`, invalidated on `snapshot_serial` and config change.
- `modern/draw/{tabs,pane_header}.rs`; dispatch in `render_tab_bar` (`tabs.rs`) and the header pass in `composition.rs` (section 6.4). The attention chip (`ChromeTarget::Attention`) is also wired into the `mobile.rs` header through its dispatch point (section 6.1); mobile classic golden fixtures are part of the phase.
- No server change. No geometry change.
- Tests: header tiers at 27/44/56/70 columns; junction cells never overwritten (split layouts 2x1, 2x2, nested); panes without a top border; zoomed pane; popup pane skipped; focused/unfocused/blocked/recent; tab overflow with chips; mouse: tab hit rects and pane-border drag handles unchanged; CJK member and task names by display width.
- Bench: client composition at 1 and 15+ panes with headers on.

### Phase 4: Orbit flow view

- `ClientShellOverlayKind::OrbitFlow` + `ClientShellOverlay::OrbitFlow(..)` (`state.rs`), input in `overlay_input.rs`, draw in `modern/draw/flow.rs`, model `modern/model/flow.rs`.
- Entry points: `keys.orbit_flow` (default chosen in this phase after checking the default keymap for conflicts), global menu item (`global_menu.rs`), task row context menu (`context_menu.rs`), and `ui.orbit_flow` (bool, default true). Config reference rows for both; help overlay lists the shortcut in modern only.
- Data: `ClientShellSnapshot.orbit` (`steps`, `step_total`); optional fields only if something is still missing after phase 2 (decided by the section 7 review, not now).
- Tests: overlay open/close/focus restore; navigation with keyboard and mouse; width tiers at 80/120/200; stacked vs side-by-side parallel; relay with 8 and with 40 steps (totals and member/kind still shown, focus on an identity-only step); fan-out of 4 with the window cut at 8; empty `steps`; resize while open rebuilds `FlowModel`; snapshot update while open; reduced/off motion; light/dark.

### Phase 5: notifications, toasts, status line, CLI

- `modern/draw/{toast,status}.rs`: modern toast card, mode bar chip style, attention chip. Event kind → glyph/state mapping built where the notification is created (`notification_policy.rs`), not per frame.
- `src/cli/orbit_activity.rs`: dispatch at the render entry (section 6.1; style and glyphs from `Config::load()`), lanes, glyphs, shared `age` from `ui::chrome::age`; `ui.glyphs` honored; classic output unchanged (CLI golden tests).
- P15 (`status.rs` byte-length widths): separate `fix:` commit before the phase, applies to both styles (R9).
- Tests: toast geometry at short areas, all positions, reduced/off, CJK titles; mode bar at 50/64/80/120 columns; CLI output golden for classic and modern, TTY vs pipe, ASCII glyphs.

### Phase 6: polish

- Matrix test: widths {64, 80, 120, 200} x themes {catppuccin, latte, terminal, one-dark, solarized-light, gruvbox, vesper, nord, dracula} x animations {full, reduced, off} x glyphs {unicode, ascii} for every modern surface; assert no overflow beyond the area, no layout shift between consecutive states.
- ASCII borders for pane dividers (server-drawn) if wanted (R4); multi-endpoint sidebar adopts glyphs and roles; decide the age refresh deadline (R8); docs for `docs/next`; release-note draft.

## 9. Keeping classic pixel-identical

1. **Golden characterization fixtures first.** Before phase 1 changes any code path, generate classic buffer dumps
   from the phase-0 commit for: team sidebar (several teams, collapsed groups, 80/120/narrow widths), work board
   (active, blocked, approval, recent, expired), flat agent list, collapsed sidebar, tab bar (single/overflow/spinner/sweep), mode bar (every mode), notification card, copy feedback,
   mobile header, flow-less overlays touched by style dispatch, light + dark + terminal themes, animations full/reduced/off. A test helper `buffer_dump(&Buffer)` writes one line per row of symbols plus a second grid of
   `(fg, bg, modifiers)` hashes. Fixtures live beside the tests and **must pass unedited through every phase**.
2. **Classic bodies are not edited.** The dispatch picks a function; classic functions keep their code. The only
   edits to classic files are the dispatch `match`, moving pure helpers (`age_text`, `put_number`) to `ui/chrome`
   with the old names re-exported, and the behavior-preserving `row_index_of` change (guarded by an old-vs-new equivalence test).
3. **Style is part of every cache key** (`TeamRowsKey`, `chrome_cache`; table in section 6.3), so a live switch rebuilds, never mixes. Every surface has exactly one dispatch point (section 6.1 table), including mobile, the collapsed sidebar and the CLI, and the architecture script flags direct references outside its explicit surface/definition/wiring allowlist.
4. **No geometry in the style.** `layout.rs`, `compute_view` and `apply_pane_chrome` are not style-aware; a test
   asserts identical `ClientShellLayout`, pane rects and PTY sizes for both styles at several widths.
5. **Frozen wire contract.** Classic ignores the new fields; the frozen fixtures decode unedited.
6. **Help, settings and docs** show style-dependent entries only in modern.
7. The one intentional classic-visible change would be P15 (non-ASCII width bug), kept out of the redesign and
   proposed as its own commit (R9).

## 10. Performance plan

Frequency x cardinality for every new piece of work:

| Work | Frequency | Cardinality | Cost class |
|---|---|---|---|
| Role/glyph derivation | config or theme change | 1 | O(1), allocation allowed |
| `ModernTeam`/`Lanes` build | orbit projection change (<= 4/s) | members + tasks (tasks <= 32) | O(n), allocates |
| `TabActivity`, `AttentionSummary`, `PaneHeaders` build | per snapshot | tabs + agents + panes | O(n) once; **removes** the tabs x agents scan in modern |
| Team draw | per frame | visible rows only (`render_measured_agent_list` is windowed: it draws only the rows that fit) | O(visible rows x width), no allocation |
| Tab draw | per frame | visible tabs | O(width) |
| Pane header pass | per frame with a surface | panes (1 to 50+) | one `&str` hash lookup + O(header width) |
| Flow draw | per frame while open | identities <= 32, detailed steps <= 8 x tasks selected | O(rows x width) |

Budgets (enforced by reporting, not by wall-clock CI): modern chrome at 1 pane adds <= 10% of today's client
composition median (about +15 us), and at 15+ panes with the Orbit scenario adds <= 15% over classic in the
same run, with the 15-vs-1 ratio staying within 0.05 of classic's. Any phase that exceeds this reports it and
investigates before commit. Hidden panes: the header pass iterates only panes in the current surface; hidden
workspaces do no chrome work beyond the existing snapshot path. Snapshot growth: steps are capped, only active
tasks carry them, and the view is published only when it changes (`same_facts`).

## 11. Testing strategy

- Pure builders (`ChromeRoles`, `StateKind`, lanes, step strips, header tiering, density drop order) are unit tests with `AppState::test_new()`-style data; no PTY.
- Rendering is tested by drawing into a `ratatui::Buffer` of a fixed size and comparing `buffer_dump` text.
- Every modern surface gets the matrix: width (narrow / 80 / 120 / 200) x light and dark x animations. Mouse hit rectangles and keyboard order are asserted for each.
- Wire tests prove additivity: new JSON parses on old structs, old JSON parses on new structs, frozen fixtures untouched.
- The architecture script test and the new draw-directory rule run in CI via `python3 -m unittest scripts.test_ui_hot_path_architecture`.
- Docs: `config_reference_check.py`, `just docs-contract-test`, translation parity.

## 12. Risks and open questions

| # | Risk or question | Mitigation / proposal |
|---|---|---|
| R1 | `modern` is the default, so every user's screen changes. | `classic` is a one-line opt-out, documented in `docs/next`; release notes call it out; golden tests keep classic honest. |
| R2 | Ambiguous-width glyphs (`◐ ▲ ✓ ⚑ ● ○`) misalign on CJK-ambiguous terminals. | `ui.glyphs = "ascii"`; width test; all glyph widths come from `unicode-width`. |
| R3 | Contrast cannot be computed for `Reset` / ANSI theme colors. | Fall back to base color + bold; shape carries state; contrast test covers all RGB themes. |
| R4 | `ascii` does not reach server-drawn pane borders. | Documented scope; phase 6 option. |
| R5 | Notes and titles are worker text in a snapshot that reaches remote endpoints. | Redact + cap + active-only (section 7); same treatment as `title`/`summary`. |
| R6 | Derived roles go stale when the palette changes at runtime. | `ChromeRoles` rebuilt wherever `palette` is replaced; a theme-switch test. |
| R7 | Painting over the server's border row (selection, hyperlinks, graphics overlays). | Junction cells are never written; borders carry no content or links; add a graphics-occlusion test with a header present. |
| R8 | Ages go stale without timers. | Same behavior as today; revisit a coarse deadline in phase 6 (must not violate `animations = off` "no ticks"). |
| R9 | `status.rs` byte-width bug is a classic-visible fix. | Separate `fix:` commit, outside the redesign; maintainer decides whether it is allowed to change classic. |
| R10 | Multi-endpoint sidebar and mobile are separate renderers. | Phase 2/3 only adopt glyphs and roles there; full structure stays out of scope. |
| R11 | `state.rs`, `mouse.rs`, `team_sidebar.rs` are already large. | New code only in `modern/`; existing files grow by dispatch lines. |
| R12 | A task whose worker is `idle` but still active may look healthy. | Shown with the idle glyph in `working` so it reads as "nothing moving"; a "stalled" tint is deferred (needs a threshold decision). |
| Q1 | Default key for `keys.orbit_flow`. | Pick in phase 4 after checking the default keymap. |
| Q2 | Should `ui.pane_headers` exist (modern but headers off)? | Not in phase 3; add only if asked. |

## 13. Feasibility review (design-sol)

Reviewed against `AGENTS.md`, `composition.rs`, `wire.rs`, `protocol/endpoint.rs`,
`shell_view.rs`, `work_board.rs`, the architecture scanner and config-reference checker.
The plan is feasible subject to these implementation gates; phase 0 changes documentation only.

1. **Render purity: conditional pass.** `compose(&mut self)` already updates layout bookkeeping and hit maps;
   this is not permission to mutate models in draw helpers. Pass borrowed immutable models, the sampled clock
   and animation phase into the header writer. Rebuild on snapshot/config/theme changes outside the pane loop.
   Keep generation/revision early exits before painting. No new locks, terminal reads, I/O or heap allocation
   per pane/row/frame. Resize-dependent flow layout must be prepared in the update/layout stage.
2. **Endpoint additivity: pass for JSON only.** `endpoint::snapshot_message` serializes the snapshot into
   `ServerMessage::Json`; optional nested fields do not alter the named core codec. Do not change any frozen
   codec or append reachable enum variants. Keep fixtures and method shapes unedited and run their tests in
   phase 2. All proposed fields support `Eq`; derived nested equality makes them visible to `same_facts`.
   Construction sites include both roster/scratch builders and sidebar/work-board test helpers. Added the
   bounded step-window rule above: a simple first-eight cap is insufficient for longer relays.
3. **Architecture scanner: feasible with corrections in section 6.5.** Use `production_code`, not just
   `mask_comments_and_literals`, to exclude test modules. Current `TEST_MODULE` only recognizes unindented
   conventional test modules; extend and test indented/attributed forms if introduced. Match tokens with
   whitespace-tolerant regexes. Test comments, normal/raw literals, production after tests and actual forbidden
   calls. Avoid treating every clone as an allocation outside draw sources. This is a regression guard,
   not proof: indirect calls, buffer writers, iterator helpers and lock acquisition still need review.
4. **Benchmark: feasible, pending phase-2 measurement.** Reuse the existing fixed-geometry harness; build
   deterministic bounded Orbit fixtures outside timed samples. Measure classic/modern with the same role,
   task and pane counts, including 32 tasks, 32 projected identities and eight detailed steps per task; report fixture construction separately.
   Exercise hidden workspaces and active panes at 1 and 15+ panes. Runtime cost cannot be claimed before the
   scenario exists. Preserve retained/hidden early exits; do not rebuild models on animation ticks.
5. **Config/docs: feasible.** Enum `values` lists must match serde declaration order exactly, including
   rename rules. Add reference rows only with their Rust fields, not in phase 0. The docs contract suite
   checks release snapshot machinery, not this design document or future heading parity; run translation
   parity when configuration prose changes. Existing select-one settings controls are suitable.
6. **Classic helpers: feasible with characterization.** Actual source is
   `src/client/shell/work_board.rs`. Move the unchanged age function, constants, decimal writer and its
   clipping dependency together, adjusting visibility only. Assert seconds/minutes/hours/days, saturation,
   zero and clipped decimal output before extraction. Keep classic glyphs and roles independent of modern
   configuration. P15 remains deferred to Polaris as a separate fix; no classic-visible correction here.
7. **Flow binding: deferred intentionally to phase 4.** Follow the existing keybinding field type,
   parser/default and reference row convention; do not document it as an enum. Check all mode-specific
   bindings before selecting a default. Reuse overlay close/focus/mouse patterns, and omit flow entry points
   in classic. Lane keys need a separate namespace or typed key to avoid collisions with real role names;
   keyboard selection/reveal must use the same cached row order as drawing.
8. **Validation scope.** Config-reference check, docs contract tests and existing hot-path architecture
   tests pass on the phase-0 tree. Full Rust fmt/clippy/nextest are deferred to implementation phases because
   this commit changes only this design document. No dependencies or server commands are needed.

### Round 1 review fixes

The added optional `parallel`/`steps_omitted` JSON objects remain additive; derive `Eq` and serde on
all new nested structs and test missing values, nonempty round trips, each summary mutation and frozen
fixtures unchanged. No new codec, endpoint method or server-owned presentation state is warranted.

Corrected the identity window to retain later active steps, made the eight-detail cap unconditional,
and replaced an uncomputable relay-completed total with the active original index. Test 40-step relays
with activity beyond index 32, omitted counts on both ends, and summaries independent of either window.
Clamp counts consistently on overflow; never wrap indices. If indices exceed `u16`, mark the projection
incomplete rather than aliasing two steps to one identity. Member/kind display strings must be bounded
and sanitized at projection time too; identity-only does not imply unbounded payloads.

Corrected cache keys for palette revision, actual mobile width tier and selected flow step. Keep stable
endpoint/task/step identities across reordered snapshots; task ordinals in hit targets must resolve via
the matching retained snapshot, with stale-generation events discarded. Lane state is client-only and
its bitset avoids role-name collisions. Prepare hit storage during update/layout: existing allocating
hit vectors do not authorize a new per-row allocation path. Overlay input takes priority over shell hits;
clear targets on mouse-disable, endpoint switch and style switch. Test these transitions and target bounds.

The dispatch whitelist is feasible only as a bounded lexical guard with definition/config/module wiring
exceptions and scanner characterization tests. It cannot establish complete dispatch coverage by itself.
The existing six scanner tests pass; the proposed new whitelist remains unimplemented and unproven until
phase 1. Config-reference and seven docs contract tests pass again. No Rust code changed; no new render
benchmark was needed for this documentation correction (phase-0 medians below remain the baseline).

Phase-0 review benchmark reruns (microseconds; columns are 1 / 15 panes):

| Scenario | Before review | After documentation edits |
|---|---|---|
| Background server surface | 252 / 250 | 248 / 261 |
| Background client composition | 156 / 168 | 141 / 160 |
| Background combined | 404 / 416 | 391 / 423 |
| Active server surface | 251 / 298 | 248 / 296 |
| Active client composition | 149 / 154 | 147 / 148 |
| Active combined | 402 / 451 | 394 / 443 |

Both runs: six profiles pass; the known `native_file_render_scale_profile` fails because
CoW source retention is unavailable (exit 101). No runtime code changed, so these differences
are sampling variation, not a redesign performance result. Original design baseline remains below.

## Appendix A: baseline raw output (phase 0)

`export ZIG=.../zig-aarch64-macos-0.16.0/zig && just bench-render-scale`, release, 120x40. Six profiles pass;
`native_file_render_scale_profile` fails as described in section 2.

```text
cargo test --release --locked --bin aster render_scale_profile -- --ignored --nocapture --test-threads=1
    Finished `release` profile [optimized] target(s) in 1m 16s
...
test server::render_scale_benchmark::render_scale_profile ... background workspaces (one pane each)
  server pane surface
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        251     281     329          1.00       1.00
          15        255     270     274          1.02       0.96
          50        256     281     295          1.02       1.00
  client shell composition
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        156     179     246          1.00       1.00
          15        171     182     197          1.10       1.02
          50        176     191     218          1.13       1.07
  combined pipeline
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        409     460     530          1.00       1.00
          15        429     445     456          1.05       0.97
          50        435     460     506          1.06       1.00
background workspaces populated agent snapshots + completions + JSON framing
       panes  clients  hidden_output  construction_us  median_us  p95_us  max_us
           1        1          false                2          6       7       7
           1        1           true                2          5       6       6
           1        2          false                2          9       9       9
           1        2           true                2          9       9      11
          15        1          false               28         49      55      74
          15        1           true               26         47      56      60
          15        2          false               25         67      71      77
          15        2           true               26         67      75      76
         118        1          false              239        384     401     409
         118        1           true              243        384     404     418
         118        2          false              240        532     552     558
         118        2           true              235        517     541     572
active panes (one workspace)
  server pane surface
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        257     269     290          1.00       1.00
          15        314     345     377          1.22       1.28
          50        379     448     486          1.47       1.67
  client shell composition
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        149     156     182          1.00       1.00
          15        163     182     189          1.09       1.17
          50        165     198     254          1.11       1.27
  combined pipeline
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        407     429     473          1.00       1.00
          15        477     526     556          1.17       1.23
          50        543     642     703          1.33       1.50
active panes populated agent snapshots + completions + JSON framing
       panes  clients  hidden_output  construction_us  median_us  p95_us  max_us
           1        1          false                2          5       6       7
           1        2          false                2          9       9       9
          15        1          false               18         31      31      32
          15        2          false               18         44      48     108
         118        1          false              171        245     261     289
         118        2          false              166        316     329     337
token rules background: populated agents, rules_per_token=0
  client shell composition
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        154     172     204          1.00       1.00
          15        179     188     196          1.16       1.09
token rules background: populated agents, rules_per_token=16
  client shell composition
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        153     160     195          1.00       1.00
          15        179     190     208          1.17       1.19
token rules active: populated agents, rules_per_token=0
  client shell composition
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        153     197     225          1.00       1.00
          15        166     178     187          1.08       0.90
token rules active: populated agents, rules_per_token=16
  client shell composition
       count  median_us  p95_us  max_us  median_vs_1x  p95_vs_1x
           1        152     163     166          1.00       1.00
          15        171     177     197          1.12       1.09
...
test server::headless::tests::native_graphics::native_file_render_scale_profile ... native_file_render_scale retained=true native_files=false source_retention=false viewport=160x48 populated_panes=1 changing_images=1 image=800x480 warmup=5 samples=35 median_us=631 p95_us=689
thread 'server::headless::tests::native_graphics::native_file_render_scale_profile' (10832380) panicked at src/server/headless/tests/native_graphics.rs:1061:29:
source profile requires working CoW retention, not decoded fallback
    server::headless::tests::native_graphics::native_file_render_scale_profile
test result: FAILED. 6 passed; 1 failed; 0 ignored; 0 measured; 4283 filtered out; finished in 8.48s
EXIT 101
```

## Appendix B: Phase 2 notes (design-sonnet)

- Built: modern role cards and lane board (`src/client/shell/modern/{model,draw}`), `ChromeTarget::Lane` with `hits.chrome_targets` and `collapsed_lanes`, `TeamRowsKey` with style, glyphs, collapsed lanes and section height, the additive wire fields of section 7 (`model`, `steps`, `step_total`, `parallel`, `steps_omitted`; server side in `orbit_notifications/shell_steps.rs`), and glyph/role indirection for the flat agent list and the collapsed sidebar.
- Deviations from the plan: only the done lane truncates to the section height (`+N` on its header); the other lanes scroll as the list always has. Role "area hint" is the title of the role's oldest open task (the wire carries no role description). The modern work header is a label; lanes collapse individually.
- The modern flat list and collapsed sidebar ignore `ui.status_indicators` (modern always uses distinct shapes). The multi-endpoint sidebar, mobile layout and workspace rows keep classic drawing until phase 6.
- Benchmark: `just bench-render-scale` now prints an Orbit-populated client composition profile (4 roles x 4 members, 32 tasks) for classic and modern.

### Phase 2 validation additions (design-sol)

- Added 48 deterministic modern snapshots: 80/120 columns, mobile/collapsed, light/dark,
  full/reduced/off, Unicode/ASCII. The expanded scenes cover relay, parallel, integrating,
  permission approval and a separate blocked needs-you task. Classic fixtures remain unedited.
- Lane target storage is inline for the four headers; drawing cannot grow a new hit vector.
  Added edge/capture-disable/projection-replacement tests, density/glyph cache tests, age-bar
  bucket rendering and exact expiry tests. Expired done rows and their summary count now agree.
- Relay strips construct at most eight nodes from original indices, including explicit elision
  for omitted identities, rather than allocating the full advertised pipeline length. Server
  notes prioritize active turns, redact the complete report before selecting its first line,
  and restrict timestamps to the detail window. Indices beyond `u16` are omitted, never aliased.
- Added nested-fact sensitivity tests for model, step state/note, totals and omitted counts;
  endpoint generation-1 fixture files are unchanged.

Validation: 4,607 native tests passed (16 skipped); 14 architecture, 151 maintenance,
55 integration asset, 5 release-workflow and 7 docs contract tests passed, alongside
formatting, native clippy, config reference and translation parity. `just check`
stops at Windows lint because this workstation lacks the SDK libc configuration.
No SDK was downloaded. Both render-scale runs pass six profiles and reproduce the
known native-file CoW retention failure. Final Orbit medians (1 / 15 / 50 panes):
classic 167 / 176 / 177 us, modern 173 / 177 / 181 us. Modern adds 3.6% / 0.6% / 2.3%
against classic in the same run; its 15-to-1 scaling ratio is 1.02 versus classic 1.05.
The final run includes scheduling outliers in maxima; the reported medians stay within
the phase budgets. At validation start, Orbit classic was 169 / 169 / 170 us and modern
166 / 164 / 178 us. No new dependencies were added.

## Appendix C: Phase 3 notes (design-sonnet)

- Built: worker pane headers painted on the border row after the pane blit (`modern/draw/pane_header.rs`, plan by stored widths, junction cells never written, symbols refilled in place), `PaneHeader` records built with the team model (`modern/model/pane_header.rs`), per-snapshot `TabActivity` (`modern/model/tabs.rs`, `ClientShellState.tab_activity`) and the tab suffix (role chips, working/attention counts) drawn by `modern/draw/tabs.rs` from `render_tab_bar`.
- Header priority: gutter and state glyph, role, member, title (down to 8 cells), age, kind, model, step line; whatever the optional parts leave returns to the title.
- Recency: a faint fill under the header from the existing flash/recency phase (full and reduced keep the one-shot, off never tints; attention states keep their static glyph either way).
- Also built: the attention chip (`▲1 ⚑1 ◐3`) at the tab bar's right edge from a per-snapshot `AttentionSummary`, shown only when the tab strip keeps its minimum width. Header click targets are not added (existing pane-border targets suffice, spec 6.6); mobile shows no header (no border row).
- Cell capacity: the compositor uses its existing pane-surface copy allocation to reserve the maximum grapheme byte length cached from projected header text (modern only; classic keeps ordinary cell cloning), and the header painter never grows a string (a symbol that does not fit becomes a space). Headers write whole grapheme clusters, and never over `│`.

### Phase 3 validation handoff (design-sol)

Added a 120-case provisional header/tab snapshot matrix (80/120/200 columns,
mobile/collapsed, light/dark, full/reduced/off, Unicode/ASCII, approval/active/relay/
parallel and blocked/recent scenes), plus cache-fact and tab-scoping tests.
Classic goldens remain unedited. Three new deterministic regressions fail:

- `header_writer_never_grows_incoming_cell_strings`: the painter grows 1-byte
  incoming cell strings when writing Unicode. Capacity must be prepared during
  state/surface construction or the existing frame-copy allocation, not grown
  by the pane/frame painter. Account for full grapheme byte lengths too.
- `header_preserves_grapheme_clusters_in_names_and_titles`: iterating chars and
  dropping zero-width codepoints loses combining accents and splits ZWJ emoji.
  Use the existing unicode-segmentation dependency and display-width contracts.
- `headers_preserve_vertical_edge_stops`: `│` is absent from the protected stops.

The tab-bar-right attention chip is still missing from the phase-3 build; finish
it or explicitly report that deferral to Polaris. New header click targets are
not required by section 6.6: the existing pane-border targets must keep working.
Do not commit phase 3 until the failing regressions are fixed and the final
snapshot changes and full verification are reviewed again.

Review verification: native fmt/clippy, 14 architecture tests, config-reference
and 7 docs tests pass. Full nextest with no fail-fast: 4,630 pass, the three
regressions above fail, 16 skipped. The provisional matrix has 120 cases.
Quiet benchmark after review (1/15/50 panes): classic 164/166/173 us, modern
165/178/176 us. Six profiles pass; native-file CoW retention still fails.
The 15-to-1 ratio delta in this sample is about 0.067, above the spec 0.05
budget; repeat and investigate while fixing the header writer before commit.

A second quiet repeat measured classic 154/162/167 us and modern 157/164/165 us;
its ratio delta is about -0.007 and its absolute overhead is within budget.
This indicates sample variation; keep both results and recheck after the writer
fixes rather than treating the first ratio excursion as a proven regression.

### Phase 3 blocker resolution (design-sonnet and design-sol)

The painter now preserves whole grapheme clusters, protects `│`, and never grows
cell strings. Surface copying provisions the capacity needed by cached header
text, including family emoji longer than 16 bytes, through the existing copy
allocation; classic and popup copying keep the original cloning path. The tab-bar
attention chip is implemented. The three regression tests and the 120-case matrix
must pass in final verification; the earlier handoff findings above are historical.

Final performance review (design-sol): the writer now takes cached text widths
and encodes color/modifier values once per text run, eliminating redundant full
text scans and per-cell conversions. Both quiet runs used the final code after
compilation and validation work had stopped. Orbit medians at 1/15/50 panes:

| Run | Classic (us) | Modern (us) | 15-to-1 ratio delta |
|---|---|---|---|
| Final quiet 1 | 164 / 164 / 169 | 154 / 165 / 168 | +0.0714 |
| Final quiet 2 | 160 / 165 / 170 | 158 / 163 / 170 | +0.0004 |

Absolute overhead stays within the 10%/15% budgets in both runs. The first ratio
excursion accompanies a 10 us one-pane modern speedup and only a 1 us difference
at 15 panes; the repeat shows no large-pane regression. The excursions are
reported rather than discarded, and the redundant per-frame work was removed.
All runs pass six profiles and retain the known native-file CoW failure.

Final validation: 4,635 native tests pass (16 skipped), including all former
blockers and the 120-case snapshots; fmt/clippy, 14 architecture tests, config
reference, translation parity, 151 maintenance tests, 5 release-workflow tests,
55 integration asset tests and 7 docs tests pass. One intermediate run under
concurrent compilation hit the untouched uncertain-delivery test; it passed in
isolation and the final full rerun. `just check` stops only at Windows lint for
the missing SDK libc configuration. No SDK, crates, wire fields or frozen fixture
updates were needed. Classic goldens remain unchanged.

### Phase 3 review round 1 (design-sonnet)

- Header span is cleared to plain border (the corner cell's style) before painting, so a stale server title never shows between parts. Audit: every other modern surface (sidebar rows, lanes, tab suffix, attention chip) draws into the freshly built chrome buffer, which holds no earlier text; the pane header is the only overlay on cells that arrive with content.
- Cell capacity is reserved per pane: `PaneHeader::cell_bytes` (largest grapheme of that header) sizes only the header-row cells of panes that are on screen and have a header, inside the surface copy; content cells and off-screen workers copy at their own length.
- Recent changes have a static form that does not depend on animations: `modern/model/recent.rs` records panes that now need you or stopped working (60 s), updated when a snapshot arrives and expired by the existing coarse tick. The marker is an underline on the member name (pane header, sidebar rows and cards), on the collapsed sidebar glyph, and on a tab holding such a pane. The sidebar done lane was already static. Full and reduced additionally keep the animated fill.
- Tabs and the chip keep blocked and approval counts apart (`▲N ⚑N ◐N`, most urgent first); role cards and the collapsed sidebar already did.

Validation additions (design-sol): header reservation spans are sorted and cached
on snapshot/surface changes in surface-local coordinates. Copying uses a linear
span cursor with no per-frame scratch resize or row-by-pane scan, and keeps byte
counts as `usize` so graphemes larger than 255 bytes are not truncated. Added
coverage for cache reuse, snapshot/geometry invalidation, and such a grapheme.
Endpoint projection reset clears the recent baseline and marks; classic expiry
cleans the record without requesting a repaint. Clearing header residue also
removes inherited hyperlink and skip metadata. The new regression tests cover
all three cases; existing classic golden fixtures remain unedited.

Final round-1 verification: 4,644 native tests pass (16 skipped), including the
26 focused header/recent/classic checks. Native fmt/clippy, 14 architecture,
config reference, translation parity, 151 maintenance, 5 release-workflow,
55 integration asset and 7 docs tests pass. `just check` stops only for the
missing Windows SDK libc configuration. No SDK or dependencies were added.

Quiet Orbit medians, 1/15/50 panes: run 1 classic 167/168/179 us and modern
164/176/183 us; run 2 classic 156/163/171 us and modern 160/176/178 us. Absolute
overhead stays below the 10%/15% limits, but the 15-to-1 ratio deltas are 0.0672
and 0.0551, above 0.05. This remaining scaling limitation is reported explicitly:
reservation preparation has been moved off the frame path and row-by-pane scans
removed, while correct residue clearing adds work on worker border rows. Do not
claim the ratio budget passed. Six benchmark profiles pass in every run; the
known native-file CoW retention profile still fails. Classic and endpoint fixture
expectations remain unedited.

## Appendix D: Phase 4 notes (design-sonnet)

- Built: the Orbit flow view as an overlay (`ClientShellOverlay::OrbitFlow`, `shell/flow_view.rs`, `modern/model/flow.rs`, `modern/draw/flow.rs`). The layout (`FlowLayout`: rows, focusable step slots, summary) is built when the view opens and when its key (snapshot serial, selected task, popup width) changes, and holds indices into `snapshot.orbit`, not copies; drawing borrows the snapshot, and with the view closed nothing runs.
- Layout: lead ━▶ role header with the task progress; relay steps joined by arrows labelled with the first line of the handoff `next` text; parallel steps under `parallel D/T done` (the task-level total), as columns from 100 cells up to 4 steps and as a list otherwise or beyond; the integrator; `… N earlier/later steps` rows for steps beyond the projected list; task chips at the bottom. Member and kind are drawn before the note and the right-hand age/phase are reserved first, so truncation cuts the note first.
- Data: `OrbitShellStep` gained optional `worker_terminal_id` and `pane_id` (parallel steps use their own agent, the active step the current worker, others the roster member's terminal; `pane_id` only while the terminal sits in a pane). The client focuses a step only if that pane is also an agent in the snapshot; the integrator and stopped workers are inert. Optional JSON fields only; no frozen codec or enum changed, so `PROTOCOL_VERSION` is not bumped.
- Entry points: `keys.orbit_flow` (default `prefix+f`), the modern `WORK` label, gated by `ui.flow_view` (default true). Classic keeps the binding inert and its existing output unchanged. Modern keybinding help, the global menu and the mobile menu list the flow entry when an Orbit team is present.
- Motion: the active step uses the shared spinner under full animations; reduced and off show the static glyph with all information.


### Phase 4 validation (design-sol)

- Added a 336-scene deterministic flow snapshot matrix: relay, parallel 1/2 and
  1/4, integrating with long/multiline/Unicode notes, approval (including approval plus blocked), blocked alone, empty;
  44/80/120/200 columns, dark/light, full/reduced/off, Unicode/ASCII. Cell style
  digests accompany text. Classic and frozen endpoint fixtures remain unedited.
- Added server projection coverage for parallel ownership, the current relay
  worker, roster fallback, a terminal without a pane, and an unknown worker.
  Unassigned parallel steps no longer inherit the relay's current worker.
- Fixed classic flow gating. Flow hit rectangles use 32 inline slots rather than
  growing vectors during draw. A snapshot serial mismatch rejects stale focus
  hits until the new layout has been composed. Added clipped-layout and bounded
  hit-storage regression tests.

- Snapshot refresh now preserves the selected task id and original step index
  when task ordinals or the 32-step window change. The cache omits height and
  glyphs intentionally: rows contain indices and no wrapping, height controls
  clipping at draw time, and glyphs are borrowed on every draw. Width alone
  chooses the column/list layout. Two-line column selections reserve both rows.
- Final native validation: 4,668 nextest tests passed, 16 skipped; fmt and clippy
  passed. Architecture (14), config reference, docs contract (7), translation
  parity (4), and maintenance/integration asset suites passed. `just check`
  reaches Windows lint and stops because the workstation has no Windows SDK
  libc configuration; no platform code changed. An earlier overlapping rebuild
  invalidated a test executable; the final sequential run above is clean.
- Quiet render-scale samples with work lanes visible, 1/15/50 panes (µs):
  sample 1 classic 167/172/180, modern 167/183/186; sample 2 classic
  167/170/177, modern 166/176/182. Absolute overhead stays under the 10%/15%
  budgets. The 15:1 normalized scaling gap was +0.066 then +0.042; the repeat
  meets +0.05, and the first is consistent with the preceding phase's
  +0.055/+0.067 samples. Prior phase baseline was classic 156/163/171 and
  modern 160/176/178; no new pre-implementation baseline was captured in this
  handoff. These profiles exercise the closed overlay (normal composition),
  not a wall-clock budget for the open flow popup. Both runs passed six profiles
  and hit the documented pre-existing native_file_render_scale_profile retained
  graphics failure. No new dependencies or frozen fixture changes.


### Phase 4 review round 1

- Parallel steps focus only their recorded launched agent; integration steps
  focus the recorded integrator. Neither guesses ownership from the current
  task worker or roster. Relay ownership retains its established fallback.
- Optional JSON step `model` data is shown after kind and omitted before member
  identity when space runs out. Missing fields retain compatibility.
- Each active step projects its own live blocked/approval attention on snapshot
  change. Lone-worker identity and state are cached rather than roster-scanned
  during drawing. Live classic/flow-disable config changes close the popup.
- The inline chrome-target capacity includes all four lane headers plus WORK;
  modern help/global/mobile entries expose the view without changing classic.
- Counting-allocator tests cover modern drawing, including oversized Unicode
  graphemes. Modern cell writers keep symbols inline (oversized joined emoji
  degrade to their first scalar); classic writers preserve ratatui's full
  grapheme behavior. A separate test protects that classic representation.
- Width calculations safely omit a model wider than the u16 display counter.
  Existing snapshot and frozen endpoint fixtures are unchanged; no new crates.
- Validation: 4,682 native tests passed (16 skipped), including unchanged classic
  and modern snapshots and endpoint contracts. Fmt/clippy, 151 maintenance tests,
  architecture (14), translation parity (4), config reference and docs contract
  (7) pass. `just check` stops at Windows lint because the local Windows SDK libc
  configuration is absent; no platform code changed. An initial build exhausted
  disk space; package-scoped Cargo cleanup and nonincremental compilation let
  the final native validation complete.
- Quiet render-scale medians with lanes visible at 1/15/50 panes (µs): classic
  163/172/174, modern 167/176/180. Modern overhead is 2.5%/2.3%/3.4%; the
  normalized 15:1 gap is -0.001 and 50:1 gap +0.010, within budget. Before this
  review, the final Phase 4 sample was classic 167/170/177 and modern
  166/176/182. Six benchmark profiles pass; the documented native graphics
  CoW-retention assertion still fails. These timings cover normal composition
  with the flow closed; allocation coverage exercises the open flow draw path.

### Phase 5 build note (design-sonnet)

Built: notifications, toasts, the mode bar, the mobile header chip and the
`aster orbit activity` view in the modern language. All of it sits behind
`ui.style = "modern"`; classic output is untouched (the classic goldens pass
unedited).

- **Dispatch.** `src/client/shell/modern_toasts.rs` owns the style dispatch for
  the visible toast, mobile banner, endpoint notice, clipboard feedback, config
  diagnostic and the mode bar's modern extras; composition calls it instead of
  the classic functions. Classic arms call exactly the functions they called
  before. `notifications::card_rect` is the classic placement arithmetic moved
  into a shared function (same expressions); the modern card reuses it, so slide
  and fade behave identically.
- **Toast.** `modern/draw/toast.rs`: no box. A one-cell bar (`role_gutter`) in
  the event's state color, the state glyph, a bold title and a muted body at
  height 1 or 2, width capped at 72 and measured with `display_width`
  (CJK/emoji sized correctly), text cut with the glyph set's ellipsis. Event
  kind maps to a `StateKind` (`event_state`): needs attention is Blocked, or
  Approval when the event's pane waits on an approval (one borrowed lookup in the
  attention cache for the single visible toast, not pane-scaled); finished and
  update ready are Done; custom is Idle. Endpoint notices map rejected and
  unsupported to Failed, timeout and unavailable to Blocked. Nothing in the draw
  path allocates; opacity fades blend the same colors as classic, reduced keeps toast fades without sliding, and off uses the settled phase.
- **Mobile banner.** Same bar, glyph and title on the bottom row, body after
  ` · ` and cut with the ellipsis.
- **Clipboard feedback and config diagnostic (P15).** Chosen: fixed **only in the
  modern path**. A byte-length fix is not classic-invisible (a non-ASCII message
  gets a narrower box), so `ui/status.rs` is unchanged and the modern
  `feedback_rect`/`draw_config_diagnostic` size by display cells. The modern
  feedback is a one-row toast (Done bar and glyph); `feedback_offset` stacks it
  under a visible toast.
- **Mode bar.** `render_mode_bar` takes `Option<ModernModeBar>`; `None` is the
  classic body byte for byte. Modern: key names in the `focus` role, hints in
  `text_muted`, `NAV`/`PFX` chips and shortened hints below 60 columns, and the
  attention chip (the tab bar's `render_attention`) right-aligned, left of the
  `update ready` notice, drawn only when it fits a free cell either side. Copy
  search prompt rows draw no chip.
- **Mobile header.** The chip sits left of the tab counter on the header's first
  row when the workspace name keeps at least 12 cells; classic unchanged.
- **`aster orbit activity`.** `src/cli/orbit_activity_modern.rs`, selected by
  `style_glyphs()` (`Config::load()`: style and `ui.glyphs`). Lanes NEEDS YOU /
  WORKING / QUEUED / DONE are derived from the existing payload (blocked workers,
  task submission and report outcome); a blocked worker is the single row for
  its task or filed notice. Header line with counts, report line under DONE rows
  (outcome, redacted summary, tests, checks), ages in the TUI's units (`s/m/h/d`
  via `age_text`), glyphs from the selected set. Jobs, team, kinds, lead and the
  command footer reuse the classic renderers (now `pub(super)`; classic output is
  byte-identical, `render` is the same composition split into functions). Plain
  text, no color and no ANSI beyond what `--watch` already emitted; JSON output
  and the API payload are unchanged; no new server data.
- **Test impact.** Modern fixtures (`modern-sidebar-phase2`, `modern-headers-phase3`,
  `modern-flow-phase4`) were regenerated: the only diff is the attention chip in
  the mobile header rows. `the_tab_bar_ends_with_an_attention_chip...` now asserts
  the chip moves to the mobile header at 40 columns; the toast/clipboard graphics
  test draws feedback through the dispatching method and tolerates a one-row toast
  lying wholly over the mobile header.
- **For design-sol.** Snapshot/unit tests wanted: toast at short areas (height
  1-3), all four positions and six clipboard positions, reduced/off phases, CJK
  and ZWJ titles, ascii glyphs, light/dark; allocation-counter test over
  `draw_card`/`draw_banner`/`draw_feedback`; mode bar at 50/64/80/120 for every
  mode with and without the update notice; CLI goldens for classic vs modern,
  ascii glyphs, empty payload, >20 rows per lane, and `age` units; architecture
  script lists `modern_toasts.rs` and `orbit_activity_modern.rs` as dispatch files.


### Phase 5 validation (design-sol)

- Added 564 deterministic toast/banner/clipboard scenes (light/dark, Unicode/ASCII,
  full/reduced/off, enter/settled/fade, heights 1–3, all positions) with cell style
  digests; 160 mode-bar scenes at 50/64/80/120 columns for every mode and update
  notice state; 18 CLI goldens for classic/modern/ASCII, empty/lanes/>20 rows and
  watch/non-watch. Existing classic fixture is unchanged. Older modern fixture
  changes are confined to the intended mobile-header chip.
- Geometry tests exercise narrow widths and oversized offsets. Three tests first
  failed on implementation bugs: top clipboard offsets could draw beyond the
  area (and bottom offsets overflow), the ASCII banner used a Unicode separator,
  and config diagnostics used an allocating classic writer. Fixed all three in
  modern paths. Allocation counting now covers card, banner, feedback and config
  diagnostic drawing cold with CJK and long ZWJ graphemes.
- Added copy-search exclusion, diagnostic display-width/occlusion and CLI age
  boundary coverage. English/Japanese/Chinese draft docs describe the surfaces
  and CLI style dispatch. No runtime data, endpoint fields or crates added.
- Before test/review fixes, the quiet standard render-scale profile measured
  classic 157/167/176 µs, modern 158/176/175 µs at 1/15/50 panes. The normalized
  15:1 gap was +0.0502 (slightly over +0.05); final samples follow below.
- First native verification: 4,703 nextest tests pass, 16 skipped; no retry was
  needed for `active_retry_failure`. Fmt/clippy, 151 maintenance tests, integration
  assets, architecture (14), config reference, translation parity (4), and docs
  contract (7) pass. `just check` reaches Windows lint and stops at the missing
  workstation Windows SDK libc configuration; no platform code changed.
- Known retained behavior: bottom notifications and feedback can share the mode
  bar's row, as before. This phase preserves classic placement and adds bounded
  geometry for modern feedback; it does not redesign the overlay stacking order.

- A real-payload CLI regression uses terminal ids distinct from display names.
  Blocked-notice deduplication now joins the existing agent pane/terminal mapping
  instead of comparing an id with a name; no server data is added.
- The final sequential verify run passes 4,713 native tests (16 skipped), including
  that regression and concurrently added backend tests. Fmt/clippy, maintenance,
  integration assets and architecture pass; Windows lint remains unavailable.
  Unrelated parallel-checkout edits briefly broke a benchmark rebuild, then were
  completed by their owner. Those backend files are excluded from this commit.
- Final quiet render-scale medians at 1/15/50 panes (µs): classic 166/175/177,
  modern 166/181/185. Modern overhead is 0%/3.4%/4.5%; normalized 15:1 gap
  +0.0361 (before +0.0502, change -0.0140), 50:1 gap +0.0482. Both fit +0.05.
  An earlier post-review sample was noisy: classic 169/175/171, modern
  163/186/188, 15:1 gap +0.1059; the final repeat is recorded rather than
  claiming every sample met budget. No pane-scaled work was added by this phase:
  the profile has no visible toast or mobile header, and the mode-bar early exit
  remains. Cold allocation tests separately cover the new toast draw paths.
  All completed standard bench runs pass six profiles and hit only the documented
  native_file_render_scale_profile CoW-retention failure.
