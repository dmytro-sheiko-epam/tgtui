# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo test                                  # whole suite (46 tests, all unit tests inside src/)
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

## Scope

Plain-text reading and sending only. Media is labelled (`[photo]`, `[file]`, …) in
`chat_buffer::media_label`, never downloaded. `grammers_client::media::Media` is
`#[non_exhaustive]`, so keep the catch-all arm. Editing, deleting, reactions, and multiple
accounts are deliberately out of scope.

Dependencies pinned with `=` (`crossterm`, `grammers-client`, `grammers-session`) are pinned
because grammers is pre-1.0 and its API moves between patch releases; bumping them means
expecting breakage in `src/telegram/`.

## Style

The existing code comments the *why*, not the *what* — a comment earns its place by explaining a
non-obvious constraint (protocol quirk, ordering requirement, race). Tests are named as full
sentences describing the behaviour they protect (`a_peerless_deletion_leaves_channels_alone`) and
assert with a message explaining what would break. Match that.
