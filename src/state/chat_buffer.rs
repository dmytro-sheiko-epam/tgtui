//! Per-chat message cache and the pagination state that drives infinite scroll.

use std::collections::{HashMap, HashSet, VecDeque};

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
    /// What the transcript prints: the message's text with media and service actions flattened
    /// into it, so `[file] here you go` is one string.
    pub text: String,
    /// What the message actually says, with nothing added.
    ///
    /// Kept apart from `text` because the flattening above is one-way and an edit has to send the
    /// message back: editing `[photo] on the left` would write the label into the caption. Equal to
    /// `text` for a plain message, which is most of them.
    pub raw_text: String,
    pub date: DateTime<Utc>,
    /// `Some` when this message can be shown as a picture rather than a label.
    pub photo: Option<PhotoRef>,
    /// The message this one is a reply to, if any. Only the id: Telegram sends the parent's
    /// content in the reply header sometimes and not others, so it is never relied on and the
    /// parent is looked up instead.
    pub reply_to: Option<i32>,
}

/// A reply's parent, reduced to the one line the transcript quotes it on.
#[derive(Debug, Clone)]
pub struct ReplyPreview {
    pub sender: Option<String>,
    pub text: String,
}

impl ReplyPreview {
    pub fn of(message: &ChatMessage) -> Self {
        Self {
            sender: message.sender.clone(),
            text: message.text.clone(),
        }
    }
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
            raw_text: msg.text().to_string(),
            date: msg.date(),
            photo,
            reply_to: msg.reply_to_message_id(),
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
    /// Parents of replies that are not themselves in `messages` — scrolled past, or never paged
    /// in at all. Fetched by visibility, exactly as photos are.
    pub reply_previews: HashMap<i32, ReplyPreview>,
    /// Ids already asked for. Set when the request goes out and *never* cleared, including for a
    /// parent the server says is gone: the trigger fires again on the very next frame, so a guard
    /// that reopened would ask forever. Same terminal shape as `PhotoState::Failed`.
    pub reply_requested: HashSet<i32>,
    /// The message the cursor is on, and so whether select mode is on at all.
    ///
    /// A message *id* rather than an index into `messages`: `prepend_older` shifts every index by a
    /// whole page and `remove` shifts them by however many went, so an index would silently come to
    /// mean a different message. `Some` is the mode — there is no second flag to keep in step.
    pub selected: Option<i32>,
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
            reply_previews: HashMap::new(),
            reply_requested: HashSet::new(),
            selected: None,
        }
    }

    /// The one-line quote to show above a reply, from the buffer first and the fetched previews
    /// second. `None` means it is not here *yet* — the caller reserves the row either way.
    pub fn reply_preview(&self, id: i32) -> Option<ReplyPreview> {
        self.messages
            .iter()
            .find(|message| message.id == id)
            .map(ReplyPreview::of)
            .or_else(|| self.reply_previews.get(&id).cloned())
    }

    /// Parents worth asking the server for: not already held, and not already asked for.
    pub fn unfetched_replies(&self, ids: &[i32]) -> Vec<i32> {
        let mut wanted: Vec<i32> = ids
            .iter()
            .copied()
            .filter(|id| self.reply_preview(*id).is_none() && !self.reply_requested.contains(id))
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        wanted
    }

    /// Put the cursor on the newest message, entering select mode. `false` in an empty chat, where
    /// there is nothing to select and the mode would be on with nothing to show for it.
    pub fn select_newest(&mut self) -> bool {
        self.selected = self.messages.back().map(|m| m.id);
        self.selected.is_some()
    }

    /// Move the cursor `delta` messages, clamping at either end rather than wrapping — the same
    /// choice the picture viewer makes, and for the same reason: a transcript has a top and a
    /// bottom, and running off one into the other would lose the reader's place.
    ///
    /// Reports whether the cursor is now on the oldest message we hold, which is the caller's cue
    /// to page in more history.
    pub fn select_step(&mut self, delta: isize) -> bool {
        let Some(current) = self.selected else {
            return false;
        };
        let Some(at) = self.messages.iter().position(|m| m.id == current) else {
            // The selected message went while the cursor was on it and nothing put it back.
            return self.select_newest() && self.messages.len() == 1;
        };

        let last = self.messages.len().saturating_sub(1);
        let moved = at.saturating_add_signed(delta).min(last);
        self.selected = self.messages.get(moved).map(|m| m.id);
        moved == 0
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
        self.selected = None;
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
    ///
    /// A cursor sitting on one of them leaves with it. Moving it to a neighbour instead would put
    /// the highlight somewhere the user never pointed it, one keystroke away from a second delete.
    pub fn remove(&mut self, ids: &[i32]) -> bool {
        let before = self.messages.len();
        self.messages.retain(|message| !ids.contains(&message.id));
        if self.selected.is_some_and(|id| ids.contains(&id)) {
            self.selected = None;
        }
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
        // The parents these quoted are gone with everything else, and so is the point of having
        // asked for them.
        self.reply_previews.clear();
        self.reply_requested.clear();
        self.has_more_older = false;
        self.loading_older = false;
        self.loaded = true;
        self.scroll = 0;
        self.selected = None;
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
    fn the_cursor_starts_on_the_newest_message() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(page(30, 3));

        assert!(buffer.select_newest());
        assert_eq!(buffer.selected, Some(30));
    }

    #[test]
    fn an_empty_chat_has_nothing_to_put_a_cursor_on() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(Vec::new());

        assert!(
            !buffer.select_newest(),
            "select mode must not turn on with no message to highlight"
        );
        assert_eq!(buffer.selected, None);
    }

    #[test]
    fn the_cursor_clamps_at_both_ends_instead_of_wrapping() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(page(30, 3));
        buffer.select_newest();

        assert!(!buffer.select_step(-1));
        assert_eq!(buffer.selected, Some(29));
        assert!(
            buffer.select_step(-5),
            "stepping past the oldest lands on it and says so"
        );
        assert_eq!(buffer.selected, Some(28));

        buffer.select_step(9);
        assert_eq!(
            buffer.selected,
            Some(30),
            "stepping past the newest must clamp, not wrap round to the top"
        );
    }

    /// The whole reason the cursor is an id: a page of older history shifts every index by 50 at
    /// once, and an index-based cursor would come to mean a different message without moving.
    #[test]
    fn an_older_page_does_not_move_the_cursor() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(page(30, 3));
        buffer.select_newest();
        buffer.select_step(-1);

        buffer.prepend_older(page(27, 3));

        assert_eq!(buffer.selected, Some(29));
        buffer.select_step(-1);
        assert_eq!(
            buffer.selected,
            Some(28),
            "the cursor must still be walking the same messages it was before the page landed"
        );
    }

    #[test]
    fn deleting_the_selected_message_takes_the_cursor_with_it() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(page(30, 3));
        buffer.select_newest();

        buffer.remove(&[30]);

        assert_eq!(
            buffer.selected, None,
            "a cursor left pointing at a message that is gone would be a mode with nothing on \
             screen to show for it"
        );
    }

    #[test]
    fn deleting_some_other_message_leaves_the_cursor_alone() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(page(30, 3));
        buffer.select_newest();

        buffer.remove(&[28]);

        assert_eq!(buffer.selected, Some(30));
    }

    #[test]
    fn clearing_the_history_puts_the_cursor_away() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(page(30, 3));
        buffer.select_newest();

        buffer.clear();

        assert_eq!(buffer.selected, None);
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
