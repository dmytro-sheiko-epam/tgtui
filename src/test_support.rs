//! Fixtures for tests. Lets the state machine be exercised without a Telegram connection.

use chrono::{TimeZone, Utc};
use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use tokio::sync::mpsc;

use crate::app::App;
use crate::state::chat_buffer::ChatMessage;
use crate::state::dialog_list::DialogSummary;
use crate::telegram::TgCommand;

pub fn peer(id: i64) -> PeerRef {
    PeerRef {
        id: PeerId::user_unchecked(id),
        auth: PeerAuth::default(),
    }
}

/// A broadcast channel or supergroup, whose message ids restart at 1 per channel.
pub fn channel(id: i64) -> PeerRef {
    PeerRef {
        id: PeerId::channel_unchecked(id),
        auth: PeerAuth::default(),
    }
}

pub fn message(id: i32, text: &str) -> ChatMessage {
    ChatMessage {
        id,
        outgoing: false,
        sender: Some("Alice".to_string()),
        text: text.to_string(),
        date: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
    }
}

/// A page as the actor delivers it: newest first.
pub fn page(newest_id: i32, count: i32) -> Vec<ChatMessage> {
    (0..count)
        .map(|offset| {
            let id = newest_id - offset;
            message(id, &format!("message {id}"))
        })
        .collect()
}

pub fn dialog(id: i64, name: &str) -> DialogSummary {
    DialogSummary {
        peer: peer(id),
        name: name.to_string(),
        preview: format!("last from {name}"),
    }
}

/// An app plus the receiving end of the commands it issues.
pub fn app() -> (App, mpsc::UnboundedReceiver<TgCommand>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (App::new(tx), rx)
}

/// Drain every command queued so far.
pub fn drain(rx: &mut mpsc::UnboundedReceiver<TgCommand>) -> Vec<TgCommand> {
    let mut commands = Vec::new();
    while let Ok(command) = rx.try_recv() {
        commands.push(command);
    }
    commands
}
