# tgtui

A terminal Telegram client: a list of your chats, the message history of the selected one with
infinite scroll, and a box to type replies. Built on [grammers] (pure-Rust MTProto) and
[ratatui].

## Running

```sh
cargo run --release
```

On first run you'll be asked for your phone number, the confirmation code Telegram sends to your
other devices, and your cloud password if the account has two-factor auth enabled. The session is
saved afterwards, so later runs go straight to your chats.

## Keys

| Key | Action |
| --- | --- |
| `↑` / `↓`, `j` / `k` | Move through the chat list |
| `Enter` | Open the selected chat and jump to the message pane |
| `Tab` | Switch between the chat list and the message pane |
| `Esc` | Back to the chat list |
| `↑` / `↓`, `PgUp` / `PgDn` | Scroll the transcript (older messages load as you reach the top) |
| *typing* | Compose a message (message pane focused) |
| `Enter` | Send the composed message |
| `Ctrl+C` | Quit |

## Scope

Reading and sending **plain text**. Media messages appear as labels (`[photo]`, `[file]`, …)
rather than being downloaded. Editing, deleting, reactions, and multiple accounts are out of
scope.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `TG_API_ID` | `17349` | Telegram API ID |
| `TG_API_HASH` | Telegram Desktop's | Telegram API hash |
| `TGTUI_LOG` | `warn` | `tracing` filter, e.g. `TGTUI_LOG=debug` |

The defaults are the publicly known Telegram Desktop credentials, which ship in its open source
and are widely reused by third-party clients. They are not officially sanctioned, so Telegram
could rate-limit or revoke them; get your own at <https://my.telegram.org> if that becomes a
problem.

The session database and log files live in the platform data directory — on macOS that is
`~/Library/Application Support/tgtui/`. Delete `tgtui.session` there to sign out.

Logs go to a file rather than the terminal, since the TUI owns stdout while it runs.

[grammers]: https://github.com/Lonami/grammers
[ratatui]: https://ratatui.rs
