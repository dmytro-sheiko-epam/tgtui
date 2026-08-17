# Peer info screen

Show the account information Telegram holds about the conversation under the cursor: a full-screen
profile with the peer's avatar, name, bio and the handful of fields worth reading from a terminal.

Opened from the existing `Ctrl+A` chat action menu, as an `Info` entry. Read-only — it is the first
entry in that menu that asks nothing of the account.

## Scope

Covers all three peer kinds, because the menu is offered on every row and an entry that did nothing
on some of them would be worse than no entry.

| Peer kind | Call | Fields used |
| --- | --- | --- |
| User | `users.getFullUser` | `about`, `blocked`, `common_chats_count`, `birthday`, `profile_photo` |
| Basic group | `messages.getFullChat` | `about`, participant count, `chat_photo` |
| Megagroup / channel | `channels.getFullChannel` | `about`, `participants_count`, `online_count`, `admins_count`, `slowmode_seconds`, `chat_photo` |

`PeerKind`'s three variants line up one-to-one with those three calls — `User` → `users.getFullUser`,
`Chat` (a basic group) → `messages.getFullChat`, `Channel` → `channels.getFullChannel`, which a
megagroup takes correctly because it *is* a `Channel`. So the actor dispatches on the peer it was
already handed and the command carries nothing else.

grammers 0.10 wraps none of the three, so all three are raw `client.invoke`s. That is the
established path here: `messages.getDialogFilters`, `contacts.block` and the hand-rolled archive
`messages.getDialogs` are all raw already.

Out of scope: participant lists, editing anything about a peer, resolving a linked chat or personal
channel into a second request, profile photo history beyond the current picture, and opening the
avatar in the full-screen picture viewer.

## Architecture

The four-step shape the rest of the app uses: new `TgCommand` → actor arm → new `TgEvent` →
`App::handle_event` arm → render.

```
Ctrl+A ─> chat menu ─> Info ─> App::open_peer_info
                                    │  peer_info = Some(PeerInfoView { state: Loading })
                                    ▼
                          TgCommand::LoadPeerInfo { peer }
                                    │
                              telegram::Actor  (one of three raw invokes)
                                    │
                          TgEvent::PeerInfoLoaded { peer, info }
                                    ▼
                          App  ─>  InfoState::Ready | Failed
                                    │
                          TgCommand::DownloadAvatar  (once, if there is a photo)
                                    ▼
                          TgEvent::AvatarLoaded ─> PhotoState::Ready
```

### Commands and events

```rust
// telegram/commands.rs
/// Read everything Telegram will say about one peer. Which of the three full-info calls this
/// becomes is decided from the peer's own kind, so nothing else has to be carried.
LoadPeerInfo { peer: PeerRef },

/// The current profile picture. Kept apart from `DownloadPhoto` on purpose — see below.
DownloadAvatar { peer: PeerRef, source: Box<PhotoSource> },
```

```rust
// telegram/events.rs
PeerInfoLoaded {
    peer: PeerId,
    /// Boxed for the reason `DownloadPhoto` boxes its source: inline, a whole profile would make
    /// every event in this channel as large as the largest one.
    ///
    /// `Err` carries what to print in place of the fields. A profile can be refused outright —
    /// privacy settings, a channel we have been kicked from — and the screen has to leave its
    /// loading state either way.
    info: Result<Box<PeerInfo>, String>,
},

/// `None` when the download failed, in the success shape for the same reason `PhotoLoaded` is:
/// the in-flight guard has to clear whichever way it went.
AvatarLoaded { peer: PeerId, image: Option<Arc<DynamicImage>> },
```

### State

Approach A: transient screen state, fetched fresh on every open, dropped on close. There is no
cache of profiles anywhere.

```rust
// app.rs
/// The profile being read. Modal and full screen, like `viewer`: while it is set the two panes
/// are not drawn at all.
pub peer_info: Option<PeerInfoView>,

pub struct PeerInfoView {
    /// Kept for the avatar download, which needs the access hash.
    pub peer: PeerRef,
    /// From the dialog row, so the title is right before the fetch lands.
    pub name: String,
    pub state: InfoState,
    /// Lines scrolled past the top of the profile. See the scroll-direction invariant.
    pub scroll: u16,
}

pub enum InfoState {
    Loading,
    Ready(Box<PeerInfo>),
    Failed(String),
}
```

Rejected alternatives, and why:

- **A cache keyed by `PeerId`.** Makes reopening instant, at the price of a second staleness
  problem — a bio edited on another device — and a show-old-then-swap flicker. One un-paged round
  trip is cheap, and nobody reopens the same profile repeatedly in a terminal client.
- **Fields on `DialogSummary`.** That struct exists for *every* conversation including ones never
  opened, so it would carry bios and avatars for hundreds of chats. Worse, `from_raw` builds it
  from archive TL that has none of these fields, so the two constructors would disagree about what
  the struct means.

### The info table

`state::peer_info` is pure: given a TL response it answers with what to draw and holds nothing.
Same role `state::dialog_actions::actions_for` plays, and for the same reason — it makes the table,
which is the thing a reader wants to check, testable on its own.

```rust
pub struct PeerInfo {
    /// Lines under the name: "@alice · verified", "last seen 2 hours ago",
    /// "1 234 members · 56 online". Empty lines are never pushed.
    pub subtitle: Vec<String>,
    /// Bio or channel description. Apart from `rows` because it wraps as a paragraph rather than
    /// sitting in a label column.
    pub about: Option<String>,
    pub rows: Vec<InfoRow>,
    pub avatar: Option<PhotoRef>,
}

pub struct InfoRow {
    pub label: &'static str,
    pub value: String,
}
```

Rows by kind:

- **User** — Phone, Birthday, Groups in common, Peer id.
- **Basic group** — Members, Peer id.
- **Megagroup / channel** — Members, Online, Admins, Link, Slow mode, Peer id.

A row whose field the server did not send is not pushed, so no screen shows a label with nothing
beside it.

Deliberately omitted, each pinned by a test:

- **Invite link.** Admin-only, and it is a credential: anything that reads it can join the chat.
- **Linked chat and personal channel.** Both arrive as bare ids. Showing the number would be
  useless and resolving it means a second request for a line nobody asked for.
- **Wallpaper, business hours, gift and star counts, ratings.** Present in the layer; nothing to do
  with reading a conversation from a terminal.

### Blocked state

`userFull.blocked` is applied to the peer's `DialogSummary`. That flag is otherwise seeded from a
single `contacts.getBlocked` page, so past the first page a blocked user currently shows "Block".
This is a server answer rather than an optimistic guess, so applying it is consistent with the rule
that chat state changes only on confirmation.

### Rendering

`ui::peer_view::render` is a pure function of `App` like the rest of `ui`: avatar boxed top-left,
subtitle lines beside it, `about` wrapped below, rows in two columns sized to the widest label,
`esc  close` at the foot.

`ui::draw` grows one arm beside the viewer's:

```rust
Screen::Main if app.viewer.is_some()    => photo_view::render(..),
Screen::Main if app.peer_info.is_some() => peer_view::render(..),
```

### Avatar plumbing

`ImageStore`'s cache key stops being a bare message id:

```rust
pub enum ImageKey { Message(i32), Avatar(PeerId) }
// HashMap<(i32, Size), Cached>  ->  HashMap<(ImageKey, Size), Cached>
```

`ImageStore::prepare` takes the same key. Nothing else about the store changes.

The avatar comes out of the profile response itself: `userFull.profile_photo`,
`chatFull.chat_photo` and `channelFull.chat_photo` are all `tl::enums::Photo`, and
`grammers_client::media::Photo::from_raw` is public, so there is no extra round trip to *discover*
the picture — only to fetch it.

`state::media` gains one function for this, `avatar_ref(&Photo) -> Option<PhotoRef>`, sitting beside
`photo_ref` and sharing its `pick_thumb` (currently private, and staying private). Both then agree on
what a terminal-sized source is, the way `is_muted` is shared by the dialog seed and the live update.
A `photoEmpty` yields no thumbs, so it returns `None` and the header draws as text — no special case.

The download is its own command/event pair rather than a widening of `DownloadPhoto`. The
transcript's path is tuned around three things the avatar does not have: a visibility trigger that
fires every frame, the `MAX_PHOTO_DOWNLOADS` cap, and the `decoded` eviction queue. There is exactly
one avatar, it is always on screen, and it dies with the screen — so it is requested once, when
`PeerInfoLoaded` lands with a `Pending` avatar, and `PhotoState` guards it as usual.

### Key handling

`handle_main_key` claims the info screen immediately after the viewer and ahead of the forward
picker and both menus. `handle_peer_info_key` takes `↑`/`↓` to scroll and `Esc` to close, and
swallows everything else.

`DialogAction::in_progress` becomes `Option<&'static str>`, returning `None` for `Info` — exactly as
`MessageAction::in_progress` already does for Reply, Edit and Forward, which are also entries that
aim the UI rather than issue a request.

`Info` sorts first in the chat menu. The order there runs reversible-things-first and
destructive-last so a mistyped `Enter` lands somewhere harmless; Info changes nothing at all.

## Invariants

- **A late answer must not land on the wrong profile.** The `PeerInfoLoaded` and `AvatarLoaded`
  reducers drop any answer whose peer is not the one currently open. Same discipline as a message id
  not outliving the chat it came from.
- **The viewer and the info screen can never both be open.** Not enforced by a check, but true by
  construction: with a picture open the chat list is not drawn, so `Ctrl+A` is unreachable and no
  `Info` entry can be chosen; with info open, `handle_peer_info_key` swallows `Ctrl+P`.
- **The info screen scrolls from the top.** `PeerInfoView.scroll` counts lines scrolled *past*,
  clamped by the renderer writing back — the same direction `metrics` and the clamped `buffer.scroll`
  already flow. This is the opposite of `ChatBuffer.scroll`, which counts up from the bottom, and
  deliberately so: a profile is a fixed-length document read top-down, while a transcript grows at
  the end and must not move when older history is prepended.
- **The avatar claims its rows before it arrives.** `ImageStore::reserve` before the download and
  `ImageStore::prepare` after it share `fit`, so the box is the same size either way and the fields
  below it never jump. A peer with no photo reserves nothing, which is known before the first draw.
- **A profile must not outlive its conversation.** `App::forget_dialog` clears `peer_info` when it
  names the peer that went, as it already clears the chat buffer and the compose box.

## Error handling

| What fails | Result |
| --- | --- |
| The fetch — network, flood wait, `CHANNEL_PRIVATE`, privacy restrictions | `Err(String)` → `InfoState::Failed`, printed in the body. Title and `Esc` keep working. It lives on the screen rather than in the status banner, because the banner is transient and this screen is not. |
| The avatar download | `PhotoState::Failed`. The reserved box keeps its rows and shows the peer's initials dimmed; collapsing it would shift every field below. |
| No graphics protocol (`TGTUI_IMAGES=off`, or the terminal query failed) | `reserve` returns `None`, no box, header is text only. Falls out of existing code. |
| Peer has no profile photo | No box reserved, header is text only. |
| Terminal too short for the header | Guard as `forward_picker` does with `inner.height < 2`: draw the fields rather than a clipped half-avatar. |
| No chat selected when `Ctrl+A` is pressed | Existing "no chat selected" status; the menu never opens, so neither does this. |

## Testing

All tests live in `#[cfg(test)] mod tests` beside the code they cover, driven by `test_support` as
the rest of the suite is. Named as full sentences, asserting with a message saying what would break.

**`state::peer_info`** — the table, where the omissions are the point:

- `a_profile_never_shows_a_row_the_server_did_not_send`
- `an_invite_link_is_never_shown_because_it_is_a_credential`
- `a_linked_chat_is_omitted_rather_than_shown_as_a_bare_id`
- `a_broadcast_channel_has_members_but_no_online_count`
- `a_bot_says_so_in_the_subtitle`

**`app`**:

- `an_answer_for_another_peer_is_dropped`
- `a_failed_profile_fetch_leaves_the_loading_state_and_says_why`
- `the_info_screen_swallows_ctrl_p_so_the_viewer_cannot_open_behind_it`
- `deleting_the_chat_closes_its_info_screen`
- `a_profile_correcting_the_blocked_flag_updates_the_dialog_row`
- `closing_the_info_screen_forgets_the_whole_profile`

**`state::dialog_actions`**:

- `every_conversation_offers_info`
- `info_is_the_one_entry_that_asks_nothing_of_the_server`

**`ui::peer_view`**:

- `the_avatar_claims_the_same_rows_before_and_after_it_arrives`
- `scrolling_clamps_at_the_end_of_the_profile`

**`state::media`**:

- `an_empty_profile_photo_yields_no_avatar_rather_than_an_empty_box`

## Documentation

`CLAUDE.md` gains, in the sections it already keeps them in:

- **Scope** — what the info screen shows per kind, and the omissions above.
- **Keys** — `Ctrl+A` → `Info`, `↑`/`↓` scroll, `Esc` closes.
- **Modal ordering** — now five deep: viewer, info screen, forward picker, chat menu, message menu,
  then the message cursor.
- **Invariants** — the scroll direction, the late-answer guard, and the note that the three
  full-info calls are raw `invoke`s because grammers 0.10 wraps none of them.
