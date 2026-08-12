# Chat actions — working notes

**Status: shipped.** `CLAUDE.md` now carries everything durable from this file; what is left here
is the working record — the API research and the rejected options. Safe to delete.

**Goal.** Give the chat list a modal action menu (`Ctrl+A`) carrying the per-conversation actions
an official Telegram client offers: mute, pin, archive, clear history, block, and delete/leave.

**Resume here:** jump to [Progress](#progress). Every step is independently compilable and
testable — `cargo test && cargo clippy --all-targets` should pass at the end of each one.

---

## Decisions already made

| Question | Answer |
| --- | --- |
| Action set | mute/unmute, pin/unpin, archive, clear history, block/unblock, delete-or-leave |
| Mark as read | **Out of scope.** Deliberate — see below |
| Menu key | `Ctrl+A`, a chord because with the message pane focused every plain character goes into the compose box (same reasoning as `Ctrl+P`) |
| Menu modality | Modal like the viewer, but a *popup*: the panes stay drawn behind it. Keys must not leak into compose |
| Optimistic updates | No. Command out, event back, then the reducer applies it — same as every other mutation in this app |

### Mark as read stays out

`tgtui` never acknowledges a read. That is load-bearing, not an oversight:
`DialogListState::reconcile_unread` takes `min` of the server's count instead of assigning it,
precisely because the server counts from a read pointer this client never moves. Opening a chat
clears the badge locally only. Nothing in this feature changes that — `client.mark_as_read` exists
in grammers 0.10 and is deliberately not called.

### Archive is a toggle (superseded)

This section used to read "Archive is one-way". It no longer is: the chat list now carries a strip
of folder tabs, one of which is the Archive, so the menu offers Unarchive on a chat that is in it
and the label is plain "Archive". Both directions are the same `folders.editPeerFolders` with a
different folder id. See the folder-tabs paragraphs in `CLAUDE.md`.

### Blocked state has to be fetched

Nothing on `dialog#fc89f7f3` says whether a user is blocked, so a Block/Unblock *toggle* needs a
seed. One `contacts.getBlocked` call after the first dialog page gives it. Only the first page
(100) is read; past that a blocked user shows "Block", and blocking twice is harmless. Live
changes arrive as `updatePeerBlocked`.

---

## API surface (all present in the pinned grammers 0.10 — verified against the vendored source)

| Action | Call |
| --- | --- |
| Delete / leave | `client.delete_dialog(peer)` — already dispatches `channels.leaveChannel` / `messages.deleteChatUser` / `messages.deleteHistory` by `PeerKind` |
| Mute | `account.updateNotifySettings{ peer: inputNotifyPeer, settings: inputPeerNotifySettings{ mute_until } }` |
| Pin | `messages.toggleDialogPin{ pinned, peer: inputDialogPeer }` |
| Archive | `folders.editPeerFolders{ folder_peers: [inputFolderPeer{ peer, folder_id: 1 }] }` |
| Clear history | `messages.deleteHistory{ just_clear: true, revoke: false, peer, max_id: 0, .. }` |
| Block | `contacts.block` / `contacts.unblock{ id: peer }` |
| Blocked seed | `contacts.getBlocked{ offset: 0, limit: 100 }` |

`client.invoke` is public, so the raw ones need no wrapper. `PeerRef: Into<tl::enums::InputPeer>`.

Seeding off the dialog page — `dialog#fc89f7f3 flags:# pinned:flags.2?true … notify_settings:PeerNotifySettings … folder_id:flags.4?int`:

- **muted** = `peerNotifySettings.mute_until` is `Some(t)` and `t > now`. Mute writes `i32::MAX`,
  unmute writes `0` (what official clients do).
- **pinned** = the `pinned` flag.
- `dialogFolder` (the archive row) carries none of these — keep it a `match`, like `read_state`.

Live updates to fold in, alongside the existing `read_event` in `src/telegram/mod.rs`:
`updateNotifySettings` (only the `notifyPeer` variant names a chat; `notifyUsers`/`notifyChats`/
`notifyBroadcasts` are category-wide — ignore them, the way `ReadChannelDiscussionOutbox` is
ignored), `updatePeerBlocked`, `updateDialogPinned`.

---

## Which actions each peer kind gets

`PeerKind` can't answer this: it collapses broadcast channels and megagroups into `Channel`.
grammers' `Peer` enum does split them (`User` / `Group` / `Channel`), which is what
`receipts_make_sense` already keys off. So capture a `DialogKind` on `DialogSummary` at
construction and derive both from it.

| | User | Saved Messages | Basic group | Megagroup | Channel |
| --- | --- | --- | --- | --- | --- |
| Mute / Unmute | ✓ | ✓ | ✓ | ✓ | ✓ |
| Pin / Unpin | ✓ | ✓ | ✓ | ✓ | ✓ |
| Archive | ✓ | ✓ | ✓ | ✓ | ✓ |
| Clear history | ✓ | ✓ | ✓ | — (admin-only) | — (not yours) |
| Block / Unblock | ✓ | — (yourself) | — | — | — |
| Delete chat | ✓ | ✓ | — | — | — |
| Leave group | — | — | ✓ | ✓ | — |
| Leave channel | — | — | — | — | ✓ |

The megagroup column is why `DialogKind::Group` carries a `megagroup` flag: `messages.deleteHistory`
does not accept a channel-shaped peer, and the `channels.deleteHistory` that would is admin-only and
deletes for everyone — a different operation from the one the entry promises.

Destructive (behind a `y`/`n` confirm): clear history, block, delete, leave.

---

## Shape of the change

Follows the house rule — new `TgCommand` → actor arm → new `TgEvent` → `App::handle_event` arm →
render. Nothing reaches for the client from `App`.

```
src/state/dialog_actions.rs   NEW   DialogAction enum, actions_for(), labels, confirm prompts.
                                    Pure and heavily unit-tested; the table above lives here.
src/state/dialog_list.rs      EDIT  DialogKind; muted/pinned/blocked fields + seeding;
                                    set_muted/set_pinned/set_blocked/remove.
src/app.rs                    EDIT  App.menu: Option<ChatMenu>; Ctrl+A; handle_menu_key;
                                    reducers for the new events.
src/ui/menu.rs                NEW   Centred popup + confirmation prompt.
src/ui/chat_list.rs           EDIT  Mute and pin markers on the row.
src/ui/mod.rs                 EDIT  Draw the popup; footer hints while the menu is open.
src/telegram/commands.rs      EDIT  SetMuted, SetPinned, Archive, ClearHistory, DeleteDialog,
                                    SetBlocked, LoadBlockedPeers.
src/telegram/events.rs        EDIT  MuteChanged, PinChanged, BlockedChanged, BlockedPeersLoaded,
                                    HistoryCleared, DialogGone { peer, reason }.
src/telegram/mod.rs           EDIT  Actor arms (all spawned); the three new updates.
src/test_support.rs           EDIT  Builders for muted/pinned/kinded dialogs.
CLAUDE.md                     EDIT  Scope section — these actions mutate real account state.
```

### Invariants this must not break

- **Removing a row must not strand the selection.** `DialogListState.selected` is a bare index.
  Deleting/archiving the last row leaves `selected` out of bounds unless it is clamped, and the
  open chat must be closed if it was the row that went. Same class of bug `bump` already guards.
- **The menu is modal.** While `App.menu` is set, `handle_main_key` routes everything to
  `handle_menu_key` — otherwise `j`/`k`/`y`/`n` fall through into the compose box. Exactly the
  reasoning in the viewer's invariant.
- **The viewer wins.** Decide the order once: viewer checked before menu, and `Ctrl+A` is ignored
  while the viewer is open.
- **Confirm before anything destructive.** Leaving a channel you can't rejoin is not undoable.

---

## Progress — complete

- [x] 1. `src/state/dialog_actions.rs` — `DialogAction`, `DialogKind`, `actions_for`, labels,
      `is_destructive`, `in_progress`, confirm text. 11 tests.
      `receipts_make_sense` moved here off `dialog_list` and now hangs off `DialogKind`.
      `DialogKind::Group` carries `megagroup`, because a megagroup's "clear history" is
      `channels.deleteHistory` — admin-only and destructive for everyone — so it is not offered.
- [x] 2. `src/state/dialog_list.rs` — `kind`/`muted`/`pinned`/`blocked` on `DialogSummary`,
      seeding via `notify_state`, `set_*` mutators, `remove` with selection clamp. 13 tests.
      Also `selected_summary`, `find`, `clear_preview`, `set_blocked_list`, and a shared
      `move_to_top` behind both `bump` and `set_pinned`. `is_muted` is public because the
      live-update path needs the same deadline rule.
- [x] 3. `src/telegram/commands.rs` + `events.rs` — seven commands, six events.
      Deleted / left / archived collapse into one `DialogGone { peer, reason }`: the list treats
      all three identically and only the wording differs.
- [x] 4. `src/telegram/mod.rs` — actor arms behind a shared `act()` helper, plus `settings_event`
      for `updateNotifySettings` / `updatePeerBlocked` / `updateDialogPinned`.
- [x] 5. `src/app.rs` — `ChatMenu`, `Ctrl+A`, `handle_menu_key`, `run_action`, `forget_dialog`,
      event reducers. 15 tests.
- [x] 6. `src/ui/menu.rs` + `chat_list.rs` + `mod.rs` — popup, `^`/`~` pin and mute markers,
      footer hints. 6 render tests.
- [x] 7. `CLAUDE.md` — scope, four new invariants, keys, test count.

**Result:** 186 tests green, `cargo clippy --all-targets` clean, `cargo fmt --check` clean.
(Baseline before this feature was 141 — `CLAUDE.md` had said 101, which was stale and is now
corrected.)

`CLAUDE.md` is the source of truth for all of this now. **This file can be deleted.**

## Not done, deliberately

- **Mark as read.** Ruled out by you. `client.mark_as_read` exists and is never called; the
  `min`-clamp in `reconcile_unread` stays correct because opening a chat still moves no pointer.
- **Editing the folders themselves.** The tabs are read with `messages.getDialogFilters` and never
  written; creating, renaming or reordering a folder stays in the Telegram app.
- **Anything needing admin rights**: adding members, changing titles, clearing a megagroup.
- **Timed mutes.** The menu mutes forever (`mute_until = i32::MAX`) or not at all.

## If you want to verify against the real account

`TGTUI_LOG=debug cargo run` — the actor logs every action's outcome, and `settings_event` logs
each settings update it decodes under `"chat settings"`. Test order of preference: mute (harmless,
reversible, and visible on a phone immediately), then pin, then clear history on Saved Messages.
Leave delete-or-leave until last.
