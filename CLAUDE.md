# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo test                                  # whole suite (347 tests, all unit tests inside src/)
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
from `App`. Two of the message actions are shorter than that: an edit and a delete already have
inbound halves (`Update::MessageEdited` and `Update::MessageDeleted`), so they need no new event
beyond one for the status banner.

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
- **The message cursor is an id, not an index.** `ChatBuffer.selected` holds a message id, because
  `prepend_older` shifts every index by a whole page and `remove` shifts them by however many went
  — an index would silently come to mean a different message. `Some` *is* select mode; there is no
  second flag that could disagree with it. `remove` clears it when the selected message is among
  the dead, and `set_initial`/`clear` clear it too: a cursor pointing at nothing would be a mode
  with nothing on screen to show for it.
- **The highlight may restyle rows but never add or remove one.** `build_transcript` draws the
  message normally and only then pads and restyles the rows it produced, which is what makes the
  cursor provably free of line accounting. `scroll` and `metrics.total_lines` are denominated in
  lines, and the cursor moves on every keystroke, so a highlight that cost a row would drift the
  viewport continuously. A photo's reserved rows are painted over by `SlicedImage` afterwards, so
  on a picture the highlight only shows on the caption or receipt line below it.
- **Scrolling the cursor into view belongs to the renderer.** The cursor steps in messages while
  the viewport moves in lines, and only the transcript just built knows which rows a message
  covers. `App` sets `scroll_to_selection` and `render_transcript` consumes it — the same direction
  `metrics` and the clamped `scroll` already flow. `event::run` draws after handling keys, so the
  correction lands in the frame the user sees rather than the one after.
- **A reply quote claims its row before the parent arrives.** `chat_view::quote_line` returns
  exactly one line whether the parent is in the buffer, still being fetched, or gone for good —
  a placeholder and a resolved quote are the same height. If the count changed when the fetch
  landed, the viewport would jump under the reader at an arbitrary moment. Same discipline as
  `ImageStore::reserve` and `prepare` sharing `fit`, and the same visibility-driven fetch as
  photos: `ChatBuffer.reply_requested` is set when the request goes out and *never* cleared,
  including for a parent the server says is gone, because the trigger fires again on the very next
  frame.
- **A message id must not outlive the chat it came from.** `App.editing` and `App.replying_to` are
  message ids, and ids repeat across conversations — so `open_selected_chat` clears both (and any
  open message menu) whenever `open_chat` actually changes, as `forget_dialog` does when the chat
  goes entirely. Carried across, they would aim the next Enter at whatever message happened to
  hold that id in the new chat. `compose` deliberately does *not* clear on a plain switch: the text
  is the user's and follows them, unlike the ids beside it.
- **`ChatMessage.text` is lossy and `raw_text` is not.** `from_grammers` flattens media labels and
  service actions into `text` so the transcript can print one string. That is one-way, so an edit
  reads `raw_text` — sending `text` back would write `[photo]` into the caption.
- **Message actions apply nothing optimistically either.** Same rule as the chat actions, for the
  same reason. A delete waits for `MessagesDeleted`; an edit's new text arrives over the update
  stream and lands in the `IncomingMessage { edited: true }` arm that already existed for edits
  made on another device. Unlike the chat menu, though, not every entry is a request: Reply and
  Edit only aim the compose box, and Forward opens a second modal — so
  `MessageAction::in_progress` returns an `Option`.
- **The forward picker searches the pool, not the folder on screen.** `DialogListState::matching`
  runs over `items` rather than `visible()`: which tab you happen to be reading has nothing to do
  with where you want to send something, and an archived chat is still a destination. Its
  `selected` indexes the *filtered* rows, so it resets to 0 on every keystroke — narrowing would
  otherwise leave it past the end.
- **The tick column is reserved, not conditional.** Outgoing bodies wrap `TICK_GUTTER` columns
  short whether or not the message has been read, so a receipt changing from ✓ to ✓✓ cannot change
  a message's line count and therefore cannot move `scroll` under the reader. Same discipline as
  `ImageStore::reserve`/`prepare` sharing `fit`.
- **The info screen scrolls from the top; the transcript scrolls from the bottom.** A profile is a
  fixed-length document read top-down, so `PeerInfoView.scroll` counts lines scrolled *past* and
  `peer_view::clamp_scroll` bounds it against the profile the frame just measured. `ChatBuffer.scroll`
  counts *up from the newest message* instead, so prepending older history does not move the
  viewport. The two look alike and mean opposite things.
- **A profile is fetched fresh and never cached.** `App.peer_info` is dropped on close, so there is
  no second staleness problem: a bio edited on another device simply arrives next time. It also
  means a late `PeerInfoLoaded` or `AvatarLoaded` must be dropped when its peer is not the one on
  screen, and that a profile must not outlive its conversation — `forget_dialog` clears it.
- **The avatar claims its rows before it arrives, and keeps them if it never does.** Same
  `ImageStore::reserve`/`prepare` pair sharing `fit` that the transcript uses. A failed download
  falls back to the peer's initials inside the box it already reserved; collapsing the box would
  shove every field below it up at an arbitrary moment.
- **The picture is fetched by the frame that reserved a box for it.** `peer_view::render` calls
  `App::request_avatar` only once `header` has a `Size`, the same shape `render_transcript` uses for
  `request_visible_photos` — no way to show it is no reason to fetch it, so with `TGTUI_IMAGES=off`
  or on a terminal with no graphics protocol the profile costs no bandwidth. `PhotoState` is the
  in-flight guard, because the trigger fires again on the very next frame.
- **An avatar's cache key is the picture, not the peer.** `ImageKey::Avatar` holds
  `tl::types::Photo.id`, carried on `state::media::Avatar`, because `ImageStore::prepare` serves a
  `(key, size)` hit without looking at the image. A peer id names whatever they are wearing today,
  so keying by it would redraw a peer who changed their photo mid-session from the encoding the old
  one left behind — contradicting the fetched-fresh rule above. `ImageKey::Message` is sound as it
  stands: a message id names one picture forever, which is the property both variants need.
- **A menu entry acts on the peer the popup named.** `run_action` hands `menu.peer` to
  `App::open_peer_info` rather than letting it re-read the selection: `forget_dialog` does not close
  an open chat menu, so a `DialogGone` arriving while it is up moves the selection to a neighbour.
  A peer whose row has since gone opens nothing at all — heading a profile with a name resolved from
  a chat that no longer exists would be a claim about the wrong conversation.
- **Not every chat-menu entry is a request.** `DialogAction::in_progress` returns an `Option`
  because `Info` only puts a screen up, exactly as `MessageAction::in_progress` does for Reply,
  Edit and Forward. A banner narrating work that is not happening would be a lie about the account.

## Scope

Reading and sending plain text, acting on individual messages (reply, edit, delete, forward), and
**displaying** pictures inline: photos, image documents,
still (WebP) stickers, and the cover thumbnail of videos and GIFs. Everything else is still
labelled (`[file]`, `[poll]`, …) by `state::media::media_label` and never downloaded — and so is
a picture whose terminal can't draw it. `grammers_client::media::Media` is `#[non_exhaustive]`,
so keep the catch-all arm. Sending media, reactions, animation, and multiple
accounts are deliberately out of scope.

Message actions live behind a cursor rather than a hotkey: `Ctrl+S` puts a highlight on the newest
message, `↑`/`↓` walk it, and `Enter` opens a menu of what that message offers. Which entries it
offers is `state::message_actions::actions_for`, and the reasoning behind each omission is in the
tests there — you cannot edit somebody else's message, a channel has no local-only delete because
`channels.deleteMessages` takes no `revoke` flag, and unsending someone else's message is only
offered where moderation exists. Formatting (Markdown/HTML) is out of scope in both directions:
the `markdown` and `html` features of grammers-client are off, so `Message::text` and
`InputMessage::text` are plain, and an edit rewrites the whole body. A reply threads on a bare
message id — quoting part of a parent, or replying across topics, would need `InputReplyTo`.
A forward carries the whole message and none of grammers' options (silent, drop author, …), all of
which it hardcodes.

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

`Ctrl+A` also opens `Info`, the one entry in that menu that asks nothing of the account: a
full-screen, read-only profile of the conversation. A user shows their handle and badges, presence,
bio, phone, birthday and how many groups you share; a basic group shows its description and member
count; a megagroup or channel adds online and admin counts and slow mode. `state::peer_info` is the
table, and the reasoning behind each omission is in the tests there — an invite link is a
credential, a linked chat arrives as a bare id worth neither printing nor a second request to
resolve, and the layer's ornaments (wallpapers, gift counts, star ratings) have nothing to do with
reading a conversation from a terminal. grammers 0.10 wraps none of `users.getFullUser`,
`messages.getFullChat` or `channels.getFullChannel`, so all three are raw `invoke`s; `PeerKind`'s
three variants pick between them exactly. Editing anything about a peer, listing participants, and
opening the avatar in the picture viewer are out of scope.

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
entries that cannot be undone from that screen ask `y`/`n` first. `Info` in that menu opens the
profile full screen, where `↑`/`↓` scroll and `Esc` closes. `Ctrl+S` puts a cursor on the
newest message in the transcript, where `↑`/`↓` walk it (clamping, like the viewer), `Enter` opens
that message's menu and `Esc` gives the keyboard back to the compose box. `Ctrl+S` is safe to
claim despite being XOFF because `ratatui::init` enables raw mode, which clears `IXON`. `Ctrl+O`
and `Ctrl+E` step forwards and backwards through the folder tabs; unlike the viewer's `←`/`→` they
wrap, because the strip is a ring whose ends are both on screen.

Five modals stack, and the order in `handle_main_key` is deliberate: viewer, peer info, forward
picker, chat menu, message menu, message cursor. The viewer is first because it is full screen and
nothing behind it is visible; the info screen is second for the same reason. The forward picker is
next because it takes plain characters into its filter, so it must be claimed ahead of the menus
that navigate with `j`/`k`. Everything modal comes before the compose box, which otherwise swallows
every letter. The viewer and the info screen can never both be open: with a picture up the chat
list is not drawn, so `Ctrl+A` is unreachable and no `Info` entry can be chosen; with a profile up,
`handle_peer_info_key` swallows `Ctrl+P`.

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
