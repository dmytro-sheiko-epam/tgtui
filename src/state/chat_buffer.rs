//! Per-chat message cache and the pagination state that drives infinite scroll.

use std::collections::VecDeque;

use chrono::{DateTime, Local, Utc};
use grammers_client::media::Media;
use grammers_client::message::Message;
use grammers_session::types::PeerRef;

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
}

impl ChatMessage {
    pub fn from_grammers(msg: &Message) -> Self {
        // Media is out of scope for this client, so it is labelled rather than downloaded.
        // A media message may also carry a caption, which is worth showing alongside the label.
        let text = match (msg.media(), msg.text()) {
            (None, text) => text.to_string(),
            (Some(media), "") => media_label(&media).to_string(),
            (Some(media), caption) => format!("{} {caption}", media_label(&media)),
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
        }
    }

    pub fn local_time(&self) -> DateTime<Local> {
        self.date.with_timezone(&Local)
    }
}

fn media_label(media: &Media) -> &'static str {
    match media {
        Media::Photo(_) => "[photo]",
        Media::Document(_) => "[file]",
        Media::Sticker(_) => "[sticker]",
        Media::Contact(_) => "[contact]",
        Media::Poll(_) => "[poll]",
        Media::Geo(_) | Media::GeoLive(_) => "[location]",
        Media::Dice(_) => "[dice]",
        Media::Venue(_) => "[venue]",
        Media::WebPage(_) => "[link]",
        // `Media` is non-exhaustive; new kinds get a neutral label rather than breaking the build.
        _ => "[media]",
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

    /// Append a message that arrived live, ignoring one we already hold.
    pub fn push_newest(&mut self, message: ChatMessage) {
        if self.messages.iter().any(|m| m.id == message.id) {
            return;
        }
        self.messages.push_back(message);
    }
}
