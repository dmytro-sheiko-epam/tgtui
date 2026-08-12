//! Per-chat message cache and the pagination state that drives infinite scroll.

use std::collections::VecDeque;

use chrono::{DateTime, Local, Utc};
use grammers_client::message::Message;
use grammers_session::types::PeerRef;

use crate::state::call::call_label;
use crate::state::media::{PhotoRef, media_label, photo_ref};

/// How many messages to request per page, both for the initial load and each scroll-up.
pub const PAGE_SIZE: usize = 50;

/// A message flattened into just what the UI renders.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub id: i32,
    pub outgoing: bool,
    pub sender: Option<String>,
    pub text: String,
    pub date: DateTime<Utc>,
    /// `Some` when this message can be shown as a picture rather than a label.
    pub photo: Option<PhotoRef>,
}

impl ChatMessage {
    pub fn from_grammers(msg: &Message) -> Self {
        let media = msg.media();
        let photo = media.as_ref().and_then(photo_ref);

        // Media that can't be drawn is labelled inline instead, alongside any caption. When it
        // *can* be drawn the label lives on the `PhotoRef` and only the caption is text, so the
        // transcript prints the label while the download is in flight and drops it once the
        // picture replaces it.
        //
        // A service message has neither text nor media of its own — only an action — and the one
        // worth spelling out is a call. Any other action stays blank, as it always has.
        let text = match msg.action() {
            Some(action) => call_label(action, msg.outgoing()).unwrap_or_default(),
            None => match (&media, msg.text()) {
                (None, text) => text.to_string(),
                (Some(_), caption) if photo.is_some() => caption.to_string(),
                (Some(media), "") => media_label(media).to_string(),
                (Some(media), caption) => format!("{} {caption}", media_label(media)),
            },
        };

        Self {
            id: msg.id(),
            outgoing: msg.outgoing(),
            sender: msg
                .sender()
                .and_then(|peer| peer.name())
                .map(str::to_string),
            text,
            date: msg.date(),
            photo,
        }
    }

    pub fn local_time(&self) -> DateTime<Local> {
        self.date.with_timezone(&Local)
    }
}

/// Everything known about one open conversation.
#[derive(Debug)]
pub struct ChatBuffer {
    pub peer: PeerRef,
    /// Ordered oldest to newest, so the view can render top to bottom.
    pub messages: VecDeque<ChatMessage>,
    /// `false` once a fetch for older messages comes back empty.
    pub has_more_older: bool,
    /// In-flight guard, so repeated scroll events don't queue duplicate requests.
    pub loading_older: bool,
    /// Whether the first page has arrived (distinguishes "loading" from "empty chat").
    pub loaded: bool,
    /// Lines scrolled up from the bottom of the transcript. 0 pins the view to the newest message.
    pub scroll: usize,
}

impl ChatBuffer {
    pub fn new(peer: PeerRef) -> Self {
        Self {
            peer,
            messages: VecDeque::new(),
            has_more_older: true,
            loading_older: false,
            loaded: false,
            scroll: 0,
        }
    }

    /// Id of the oldest loaded message, which is the offset for the next page.
    pub fn oldest_id(&self) -> Option<i32> {
        self.messages.front().map(|m| m.id)
    }

    /// Install the first (newest) page of messages.
    pub fn set_initial(&mut self, mut messages: Vec<ChatMessage>) {
        // Pages arrive newest-first; the buffer is oldest-first.
        messages.reverse();
        self.messages = messages.into();
        self.loaded = true;
        self.loading_older = false;
        self.has_more_older = !self.messages.is_empty();
        self.scroll = 0;
    }

    /// Prepend a page of older messages fetched during scroll-up.
    ///
    /// The scroll offset is measured from the bottom, so it stays correct as the top grows and
    /// the viewport does not jump.
    pub fn prepend_older(&mut self, messages: Vec<ChatMessage>) {
        self.loading_older = false;
        if messages.is_empty() {
            self.has_more_older = false;
            return;
        }
        // Pages arrive newest-first, so walking them forwards and pushing to the front
        // lands them in oldest-first order ahead of what we already have.
        for msg in messages {
            self.messages.push_front(msg);
        }
    }

    /// Drop messages that were deleted, reporting whether anything went.
    pub fn remove(&mut self, ids: &[i32]) -> bool {
        let before = self.messages.len();
        self.messages.retain(|message| !ids.contains(&message.id));
        self.messages.len() != before
    }

    /// Empty the buffer after the history was cleared on the server.
    ///
    /// Kept rather than dropped: the conversation is still open and still in the list, and a
    /// dropped buffer would send the next frame back to the network for a page that no longer
    /// exists. `has_more_older` goes false for the same reason — there is provably nothing behind
    /// this, so scrolling up must not start paginating an empty history.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.has_more_older = false;
        self.loading_older = false;
        self.loaded = true;
        self.scroll = 0;
    }

    /// Append a message that arrived live, ignoring one we already hold.
    pub fn push_newest(&mut self, message: ChatMessage) {
        if self.messages.iter().any(|m| m.id == message.id) {
            return;
        }
        self.messages.push_back(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{message, page, peer};

    fn ids(buffer: &ChatBuffer) -> Vec<i32> {
        buffer.messages.iter().map(|m| m.id).collect()
    }

    #[test]
    fn initial_page_is_stored_oldest_first() {
        let mut buffer = ChatBuffer::new(peer(1));
        // The actor hands over newest-first; the buffer renders top-to-bottom.
        buffer.set_initial(page(30, 3));

        assert_eq!(ids(&buffer), [28, 29, 30]);
        assert_eq!(buffer.oldest_id(), Some(28));
        assert!(buffer.loaded);
    }

    #[test]
    fn older_page_lands_ahead_of_what_we_hold_and_stays_ordered() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(page(30, 3));
        buffer.loading_older = true;

        buffer.prepend_older(page(27, 3));

        assert_eq!(ids(&buffer), [25, 26, 27, 28, 29, 30]);
        assert_eq!(buffer.oldest_id(), Some(25));
        assert!(!buffer.loading_older, "the in-flight guard must clear");
        assert!(buffer.has_more_older);
    }

    #[test]
    fn empty_older_page_marks_history_exhausted() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(page(30, 3));
        buffer.loading_older = true;

        buffer.prepend_older(Vec::new());

        assert!(!buffer.has_more_older);
        assert!(!buffer.loading_older);
        assert_eq!(ids(&buffer), [28, 29, 30]);
    }

    #[test]
    fn a_chat_with_no_history_is_not_asked_for_more() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(Vec::new());

        assert!(buffer.loaded);
        assert!(!buffer.has_more_older);
    }

    #[test]
    fn live_message_is_appended_once_even_if_echoed() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(page(30, 1));

        buffer.push_newest(message(31, "hello"));
        // `send_message` and the update stream both report the same message.
        buffer.push_newest(message(31, "hello"));

        assert_eq!(ids(&buffer), [30, 31]);
    }
}
