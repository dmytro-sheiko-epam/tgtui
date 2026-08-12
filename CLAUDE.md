# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo test                                  # whole suite (224 tests, all unit tests inside src/)
cargo test scrolling_near_the_top           # one test by substring of its name
cargo test app::tests                       # one module's tests
cargo clippy --all-targets
cargo fmt
cargo run --release                         # runs the real client against Telegram
```

There is no integration-test directory; every test lives in a `#[cfg(test)] mod tests` next to
the code it covers. The suite needs no network — `src/test_support.rs` builds `App` on a plain
channel, so the whole state machine is drivable by feeding `TgEvent`s and draining `TgCommand`s.

Debugging a real run: the TUI owns stdout, so `println!` is useless. Use `tracing` and read the
log — `TGTUI_LOG=debug cargo run` writes to `~/Library/Application Support/tgtui/tgtui.log`
(macOS; the platform data dir elsewhere). Delete `tgtui.session` in that directory to sign out.
The log also records which graphics protocol the terminal reported at startup. `TGTUI_IMAGES`
overrides that detection: `off` keeps media labelled, `halfblocks` skips the query.
`TGTUI_IMAGE_ROWS=<n>` caps how tall an inline picture may be; unset, the transcript gives one
two thirds of its own height.

## Architecture

Three concurrent parts joined by two unbounded channels — nothing else crosses between them:

```
crossterm keys ─┐
Telegram events ├─> event::run (select! loop) ─> App (sync reducers) ─> ui::draw
250ms tick     ─┘                                     │
                                          TgCommand   ▼
                                    telegram::Actor (tokio task, owns grammers Client)
```

- **`event::run`** (`src/event.rs`) is the only loop. Each iteration draws a full frame, then
  `select!`s over terminal input, Telegram events, and a 250 ms redraw tick.
- **`App`** (`src/app.rs`) is entirely synchronous and never awaits or does I/O. Key presses and
  `TgEvent`s are reducers over its fields; anything needing the network is expressed as a
  `TgCommand` pushed down a channel. This is what makes the state machine testable and keeps the
  render loop from ever blocking on a request.
- **`telegram::Actor`** (`src/telegram/mod.rs`) is the only code that touches
  [grammers](https://github.com/Lonami/grammers). Login steps are awaited inline in the actor
  loop (they're strictly sequential); everything else is `tokio::spawn`ed so a slow request or a
  flood-wait sleep can't stall commands queued behind it.
- **`ui::draw`** (`src/ui/`) is a pure function of `App` — every frame is rebuilt from scratch.
  It also carries an `ImageStore` (`src/ui/images.rs`), the one piece of state that belongs to
  the terminal rather than the app: the graphics protocol, the font's pixel size, and a bounded
  cache of encoded pictures. `App` holds decoded images and nothing terminal-specific.

When adding a feature that talks to Telegram, the shape is always: new `TgCommand` variant →
actor arm → new `TgEvent` variant → `App::handle_event` arm → render. Don't reach for the client
from `App`.

### Invariants worth knowing before editing

- **Page ordering.** grammers yields history newest-first; `ChatBuffer.messages` is a `VecDeque`
  stored oldest-first. `set_initial` reverses, `prepend_older` pushes to the front. Get this
  backwards and the transcript silently inverts.
- **Scroll counts up from the bottom.** `ChatBuffer.scroll` is lines above the newest message, so
  prepending older history doesn't move the viewport. Infinite scroll depends on this.
- **Rendering feeds key handling.** `chat_view::render_transcript` measures the wrapped transcript
  and writes `App.metrics` (total lines, viewport) plus a clamped `buffer.scroll` back into the
  app — that's why `ui::draw` takes `&mut App`. `App::load_older_if_needed` reads those metrics to
  decide when the view is near the top. Tests that exercise scrolling must either render a frame
  or set `app.metrics` by hand.
- **In-flight guards.** `ChatBuffer.loading_older` and `DialogListState.loading` prevent duplicate
  pagination requests; the actor reports even a *failed* older-messages fetch as an empty
  `OlderMessagesLoaded` so the guard clears and the user can retry by scrolling again.
- **Deletion peer ambiguity.** Telegram names the chat only for channel deletions. Bare ids are
  unambiguous for users and small groups (one per-account sequence) but channel ids restart at 1
  per channel, so `App::remove_messages` must skip `PeerKind::Channel` buffers for peer-less
  deletions. Four tests in `app.rs` pin this down.
- **Echo suppression.** A sent message arrives twice — once as `MessageSent`, once over the update
  stream. `ChatBuffer::push_newest` dedupes by id.
- **Live updates start late.** `stream_updates` is spawned only after the first dialog page lands,
  because update gap resolution needs the peers that fetch puts in the session cache.
- **Session file permissions.** `config::restrict_to_owner` runs on every start (0700 dirs, 0600
  files) — the session DB holds a bearer auth key. Don't add files to the data dir without it.
- **A picture must claim exactly the rows it covers.** `scroll` and `metrics.total_lines` are
  denominated in transcript lines, so `chat_view::build_transcript` pushes exactly
  `size.height` blank lines per drawn image and records a `Placement` over them. Reserve a row
  too many and the viewport drifts by one per image. `ImageStore::reserve` (before the download)
  and `ImageStore::prepare` (after) deliberately share `fit`, so the count never changes when
  the picture arrives.
- **Photos are fetched by visibility.** `render_transcript` hands `App::request_visible_photos`
  the ids on screen. `PhotoState` is the in-flight guard — only `Pending` is requested, and
  `Failed` is terminal, because the trigger fires again on the very next frame. That same call
  records what `Ctrl+P` opens, which is why the viewer must not clobber it with its own id.
- **Encodings are cached per `(message id, size)`.** The same picture is held inline *and* full
  screen. One entry per message would re-encode on every trip in and out of the viewer.
- **The viewer is modal.** While `App.viewer` is set, `handle_main_key` routes everything to
  `handle_viewer_key` and `ui::draw` skips both panes. Without that, keys would fall through to
  the compose box behind it.
- **Read state is a per-dialog watermark, not per-message.** Telegram has no "seen" flag on a
  message, only `read_outbox_max_id` per chat, so both fields live on `DialogSummary` — the one
  structure that exists for every conversation, including ones never opened. `ChatBuffer` knows
  nothing about it; `render_transcript` reads the watermark off the same dialog lookup it already
  does for the pane title. Updates apply as a *maximum*, because resolving an update gap can
  replay an older read after a newer one.
- **Read receipts are never sent.** Opening a chat clears its badge locally only — there is no
  `mark_as_read` anywhere in `src/telegram/` and no `TgCommand` that could carry one, so the
  conversation stays unread on the user's other clients. That is also why
  `DialogListState::reconcile_unread` takes the `min` of the server's `still_unread_count` rather
  than assigning it: the server counts from a read pointer tgtui never moves, so its number can
  only be believed when it is the smaller one. Assigning would resurrect a badge the user cleared.
- **Chat actions are never applied optimistically.** `App` sends the `TgCommand` and waits for the
  event; `run_action` changes nothing in `DialogListState` itself. This is the one part of the app
  whose state is shared with the user's other devices, so a mute that silently failed but showed as
  muted would be a lie about the account. It also means the same reducers serve a change made here
  and one made on a phone — `settings_event` translates `updateNotifySettings`, `updatePeerBlocked`
  and `updateDialogPinned` into exactly the events the menu produces.
- **A mute is a deadline, not a flag.** `mute_until` is the second a chat becomes noisy again;
  clients write a far-future one to mean "forever". `dialog_list::is_muted` is shared by the dialog
  seed and the live update so the two can never disagree about what the field means.
- **Removing a row must not strand the selection.** `DialogListState.selected` is a bare index
  handed straight to ratatui, and it indexes the *visible* list rather than `items` — so nothing can
  adjust it arithmetically. Every mutation that reorders, removes, refiles or refilters runs inside
  `keeping_selection`, which anchors on the selected `PeerId` and falls back to the old position
  when that conversation has left the view. `App::forget_dialog` then follows it with the chat pane
  and clears the compose box — a half-typed line must not be carried into whichever conversation
  replaces the one that went.
- **One pool, two folders, N views.** `DialogListState.items` holds the main list and the archive
  together, each row flagged `archived`, because a mute or an unread update names a chat and not a
  folder; two lists would mean every reducer looking in both, and applying twice. What is on screen
  is `visible()`, recomputed each frame — a cached membership would go stale the moment a mute
  changed and a folder that excludes muted chats would lie about what it holds.
- **Archiving moves a row; deleting removes one.** `TgEvent::FolderChanged` and
  `App::refile_dialog` keep the `ChatBuffer` and leave the chat open, because the conversation still
  exists — only its folder changed. `DialogGone` and `forget_dialog` are for delete and leave, which
  really do end it. Collapsing the two, as an earlier version did, means re-fetching a whole
  transcript to read it in the tab it moved to.
- **An absent `folder_id` is not folder 0.** It means every folder, so the main dialog fetch carries
  archived chats too and `is_archived` has to read the flag off each row. Assuming otherwise is what
  put archived chats in the "All" tab. It also makes the two cursors overlap, so `extend` dedupes by
  peer and lets the row already held win — except for `archived`, which the server has just restated
  and which nothing else reports for a chat archived elsewhere while tgtui was not running.
- **The archive cursor is hand-rolled, and skips two things `DialogIter` does.** Archived peers are
  not written into the session's peer cache and channel `pts` is not recorded for them, so an
  archived channel resolves an update gap less precisely. Neither affects reading or sending: the
  `PeerRef` built by `DialogSummary::from_raw` carries the access hash off the response itself.
  `messages.getDialogs` also has no continuation token — the next request restates the last row's
  peer, top message id and that message's date, and all three must agree.
- **The menu is modal, and the viewer outranks it.** While `App.menu` is set `handle_main_key`
  routes everything to `handle_menu_key`, or `j`/`k`/`y`/`n` would fall through into the compose box
  behind the popup. `Ctrl+A` is checked after the viewer, because with a picture open the chat list
  is not drawn and a menu over it would act on something invisible. While a confirmation is
  pending `Esc` means "no" rather than "close".
- **The tick column is reserved, not conditional.** Outgoing bodies wrap `TICK_GUTTER` columns
  short whether or not the message has been read, so a receipt changing from ✓ to ✓✓ cannot change
  a message's line count and therefore cannot move `scroll` under the reader. Same discipline as
  `ImageStore::reserve`/`prepare` sharing `fit`.

## Scope

Reading and sending plain text, and **displaying** pictures inline: photos, image documents,
still (WebP) stickers, and the cover thumbnail of videos and GIFs. Everything else is still
labelled (`[file]`, `[poll]`, …) by `state::media::media_label` and never downloaded — and so is
a picture whose terminal can't draw it. `grammers_client::media::Media` is `#[non_exhaustive]`,
so keep the catch-all arm. Sending media, editing, deleting, reactions, animation, and multiple
accounts are deliberately out of scope.

Calls arrive as service messages, which carry neither text nor media, and `state::call::call_label`
flattens them the same way: `[incoming call · 3:21]`, `[cancelled call]`, `[missed video call]`,
and, for a group's voice chat, `[video chat started]` / `[video chat ended · 12:03]`. Placing or
joining one from the terminal is out of scope. Every other service action (joined, pinned, title
changed) still renders as a blank line — extending `call_label` is where that would change.

Chat actions live behind `Ctrl+A` on the selected conversation: mute/unmute, pin/unpin,
archive/unarchive, clear history, block/unblock, and delete-or-leave. Unlike everything else in this app these change
the account's real state, visible on every other device the user owns, so they are the only
commands issued from an explicit menu choice rather than from navigation. Which entries a
conversation offers is `state::dialog_actions::actions_for`, and the reasoning behind each omission
is in the tests there — a broadcast channel has no copy of its history that is yours to clear, a
megagroup's would be `channels.deleteHistory` (admin-only, and destructive for everyone), and there
is nobody to block in a group. `client.mark_as_read` exists in grammers and is deliberately never
called; see the read-state paragraph below.

The chat list is a strip of folder tabs over one pool of dialogs: `All`, the account's own folders,
then `Archive`. `Ctrl+O` and `Ctrl+E` step through them, wrapping.

Only two of those tabs are server folders, and neither fetch maps onto one cleanly. `DialogIter`
sends `messages.getDialogs` with the `folder_id` flag *absent*, which does not mean folder 0 — it
means every folder, so the main fetch delivers archived chats mixed in and only each row's own
`folder_id` says which is which (`dialog_list::is_archived`). The archive is folder 1 and is *also*
paged on its own, by hand, through a raw `messages.getDialogs` (`Actor::load_more_archived`),
because `DialogIter`'s request is private and cannot be re-pointed — the main list pages in recency
order, so without a dedicated cursor an old archived chat would only surface after paging the whole
account. The two therefore overlap, which is why `DialogListState::extend` dedupes by peer. The account's *own* folders are not collections at all —
`messages.getDialogFilters` returns rules, and every client evaluates them over the dialog list it
already has. `state::folders::matches` is that evaluation, and it is why a custom tab keeps pulling
pages of the main list until it has rows to show: the server will not answer "the chats in Work".

Archiving is therefore a toggle, not a trapdoor. Both directions are one
`folders.editPeerFolders` with a different folder id, and the row moves between tabs rather than
leaving the pool. Blocked state is the one flag not on the dialog row, so it is seeded from a single
`contacts.getBlocked` at startup; past its first page a blocked user shows "Block", and blocking
twice is harmless. Adding a member, changing a title, editing the folders themselves, and anything
else requiring admin rights are out of scope.

Read state is display-only. Outgoing messages carry a `✓` (sent) or `✓✓` (read by the recipient)
at the right edge of their last line, and each chat-list row shows its unread count. Ticks are
suppressed in broadcast channels, which have readers rather than a recipient, and in Saved
Messages, which is you at both ends — `state::dialog_actions::DialogKind::receipts_make_sense`
decides. Nothing is
ever acknowledged back to Telegram, so the badge counts from the last time *this* client opened
the chat and reading elsewhere only ever lowers it. Marking a chat read from the terminal is
deliberately out of scope; it would change the account's real state on every other device.

Keys: `Ctrl+P` opens the newest picture on screen full screen — a modifier because every plain
character goes into the compose box — then `←`/`→` step through the chat's pictures and `Esc`
closes. Stepping clamps at either end rather than wrapping. `Ctrl+A`, a modifier for the same
reason, opens the action menu on the selected chat: `↑`/`↓` and `Enter` pick, `Esc` closes, and the
entries that cannot be undone from that screen ask `y`/`n` first. `Ctrl+O` and `Ctrl+E` step
forwards and backwards through the folder tabs; unlike the viewer's `←`/`→` they wrap, because the
strip is a ring whose ends are both on screen.

Pictures live in memory only and are never written to disk — the data directory is locked down
for the session key, and chat photos have no business outliving the process. `state::media`
picks a thumbnail sized for a terminal rather than the original, and `ui::images` caps how many
decoded and encoded copies are held at once.

Dependencies pinned with `=` (`crossterm`, `grammers-client`, `grammers-session`) are pinned
because grammers is pre-1.0 and its API moves between patch releases; bumping them means
expecting breakage in `src/telegram/`.

## Style

The existing code comments the *why*, not the *what* — a comment earns its place by explaining a
non-obvious constraint (protocol quirk, ordering requirement, race). Tests are named as full
sentences describing the behaviour they protect (`a_peerless_deletion_leaves_channels_alone`) and
assert with a message explaining what would break. Match that.
