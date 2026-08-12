//! The chat list: dialog summaries, the folder tabs drawn over them, and the cursors that lazily
//! load more of them.
//!
//! One pool, two server folders. `items` holds the main list and the archive together, each row
//! carrying an `archived` flag, because a mute or an unread update names a chat and not a folder —
//! with two separate lists every such reducer would have to look in both and could apply twice.
//! What the user sees is a view over the pool: see [`FolderTab`] and [`DialogListState::visible`].

use chrono::Utc;
use grammers_client::peer::Dialog;
use grammers_client::tl;
use grammers_session::types::{PeerId, PeerRef};

use crate::state::call::call_label;
use crate::state::dialog_actions::DialogKind;
use crate::state::folders::{self, Folder};

/// How many dialogs to pull per page.
pub const PAGE_SIZE: usize = 30;

/// Start loading more dialogs once the selection is this close to the end of the loaded list.
const PREFETCH_MARGIN: usize = 5;

/// The archive. Folder 0 is the main list; there are no other folders in the API.
const ARCHIVE_FOLDER: i32 = 1;

#[derive(Debug, Clone)]
pub struct DialogSummary {
    pub peer: PeerRef,
    /// Which of user / group / channel this is, decided once from grammers' `Peer` — `PeerKind`
    /// alone cannot tell a broadcast channel from a megagroup, and the action menu needs to.
    pub kind: DialogKind,
    pub name: String,
    pub preview: String,
    /// Highest id of ours the other side has read, or `None` where a receipt would mean nothing —
    /// see [`DialogKind::receipts_make_sense`]. Telegram never reports a per-message read flag,
    /// only this per-chat watermark, so it is the whole basis for the ✓✓ in the transcript.
    pub read_outbox_max_id: Option<i32>,
    /// Incoming messages not yet read. Seeded from the server, then maintained locally: tgtui
    /// never acknowledges a read, so this counts from the last time *this* client looked.
    pub unread: usize,
    /// Notifications silenced. Display-only here — tgtui raises no notifications to suppress —
    /// but it is the account's real setting, shared with every other client.
    pub muted: bool,
    pub pinned: bool,
    /// Whether this user is on the account's blocked list. Unlike the others this is *not* on the
    /// dialog row, so it stays `false` until `contacts.getBlocked` answers. Always `false` for a
    /// group or channel, which cannot be blocked.
    pub blocked: bool,
    /// Which server folder this came from: folder 1, the archive, rather than folder 0.
    pub archived: bool,
    /// On the account's contact list, and a bot, respectively. Neither has anything to do with how
    /// the row is drawn — they are here because [`crate::state::folders`] needs them to tell a
    /// folder of contacts from one of bots, and a dialog row is the only place they arrive.
    pub contact: bool,
    pub bot: bool,
}

impl DialogSummary {
    pub fn from_grammers(dialog: &Dialog) -> Self {
        let name = dialog
            .peer()
            .name()
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("id {}", dialog.peer_id().bare_id_unchecked()));

        let preview = dialog
            .last_message
            .as_ref()
            .map(|msg| {
                let text = msg.text();
                if text.is_empty() {
                    // Empty covers both media and service messages; a call knows what to say.
                    msg.action()
                        .and_then(|action| call_label(action, msg.outgoing()))
                        .unwrap_or_else(|| "[media]".to_string())
                } else {
                    text.lines().next().unwrap_or_default().to_string()
                }
            })
            .unwrap_or_default();

        let kind = DialogKind::of(dialog.peer());
        let (read_outbox_max_id, unread) = read_state(&dialog.raw, kind.receipts_make_sense());
        let (pinned, muted) = notify_state(&dialog.raw, Utc::now().timestamp());
        let user = match dialog.peer() {
            grammers_client::peer::Peer::User(user) => Some(user),
            _ => None,
        };

        Self {
            peer: dialog.peer_ref(),
            kind,
            name,
            preview,
            read_outbox_max_id,
            unread,
            muted,
            pinned,
            // Nothing on the dialog row says so; `BlockedPeersLoaded` fills this in later.
            blocked: false,
            archived: is_archived(&dialog.raw),
            contact: user.is_some_and(|user| user.contact()),
            bot: user.is_some_and(|user| user.is_bot()),
        }
    }

    /// The same, for an archived chat, which arrives as undressed TL rather than as a
    /// [`grammers_client::peer::Dialog`].
    ///
    /// The archive is fetched by hand — `DialogIter` hardcodes `folder_id: None` and will not be
    /// re-pointed — and the friendly types cannot be rebuilt from the answer: `Message::from_raw`
    /// wants a `PeerMap`, which has no public constructor. So this reads the same three things
    /// `from_grammers` does straight off the wire, sharing `read_state` and `notify_state` so the
    /// two paths can never disagree about what a field means.
    ///
    /// `None` for the `dialogFolder` row, which stands for a group of chats rather than one, and
    /// for a peer missing from the response's `users`/`chats` — without its access hash there is no
    /// `PeerRef` to open the chat with.
    pub fn from_raw(
        raw: &tl::enums::Dialog,
        users: &[tl::enums::User],
        chats: &[tl::enums::Chat],
        messages: &[tl::enums::Message],
    ) -> Option<Self> {
        let tl::enums::Dialog::Dialog(dialog) = raw else {
            return None;
        };
        let peer = resolve_peer(&dialog.peer, users, chats)?;

        let kind = peer.kind;
        let (read_outbox_max_id, unread) = read_state(raw, kind.receipts_make_sense());
        let (pinned, muted) = notify_state(raw, Utc::now().timestamp());

        Some(Self {
            peer: peer.reference,
            kind,
            name: peer.name,
            preview: raw_preview(&dialog.peer, dialog.top_message, messages),
            read_outbox_max_id,
            unread,
            muted,
            pinned,
            blocked: false,
            archived: true,
            contact: peer.contact,
            bot: peer.bot,
        })
    }
}

/// Everything a dialog row needs about its peer, once the response's `users` and `chats` have been
/// searched for it.
struct RawPeer {
    reference: PeerRef,
    kind: DialogKind,
    name: String,
    contact: bool,
    bot: bool,
}

fn resolve_peer(
    peer: &tl::enums::Peer,
    users: &[tl::enums::User],
    chats: &[tl::enums::Chat],
) -> Option<RawPeer> {
    match peer {
        tl::enums::Peer::User(peer) => {
            let found = users.iter().find(|user| user.id() == peer.user_id)?;
            let tl::enums::User::User(user) = found else {
                return None;
            };
            let name = [user.first_name.as_deref(), user.last_name.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            Some(RawPeer {
                reference: PeerRef::from(found),
                kind: DialogKind::User {
                    is_self: user.is_self,
                },
                name: fallback_name(name, user.id),
                contact: user.contact,
                bot: user.bot,
            })
        }
        tl::enums::Peer::Chat(peer) => {
            let found = chats.iter().find(|chat| chat.id() == peer.chat_id)?;
            let (name, id) = match found {
                tl::enums::Chat::Chat(chat) => (chat.title.clone(), chat.id),
                tl::enums::Chat::Forbidden(chat) => (chat.title.clone(), chat.id),
                _ => return None,
            };
            Some(RawPeer {
                reference: PeerRef::from(found),
                // A basic group is never a megagroup: that is the channel-shaped kind.
                kind: DialogKind::Group { megagroup: false },
                name: fallback_name(name, id),
                contact: false,
                bot: false,
            })
        }
        tl::enums::Peer::Channel(peer) => {
            let found = chats.iter().find(|chat| chat.id() == peer.channel_id)?;
            let (name, id, megagroup) = match found {
                tl::enums::Chat::Channel(chat) => (chat.title.clone(), chat.id, chat.megagroup),
                tl::enums::Chat::ChannelForbidden(chat) => {
                    (chat.title.clone(), chat.id, chat.megagroup)
                }
                _ => return None,
            };
            Some(RawPeer {
                reference: PeerRef::from(found),
                kind: if megagroup {
                    DialogKind::Group { megagroup: true }
                } else {
                    DialogKind::Channel
                },
                name: fallback_name(name, id),
                contact: false,
                bot: false,
            })
        }
    }
}

/// A deleted account has no name at all, and an unnamed row is unclickable. Same fallback the
/// friendly path uses.
fn fallback_name(name: String, id: i64) -> String {
    if name.trim().is_empty() {
        format!("id {id}")
    } else {
        name
    }
}

/// The preview line for an archived chat, read off the response's `messages`.
///
/// Matched on both peer and id: message ids restart at 1 in every channel, so `top_message` alone
/// would happily pick another chat's line out of the same response.
fn raw_preview(
    peer: &tl::enums::Peer,
    top_message: i32,
    messages: &[tl::enums::Message],
) -> String {
    let found = messages.iter().find(|message| match message {
        tl::enums::Message::Message(message) => {
            message.id == top_message && &message.peer_id == peer
        }
        tl::enums::Message::Service(message) => {
            message.id == top_message && &message.peer_id == peer
        }
        tl::enums::Message::Empty(_) => false,
    });

    match found {
        Some(tl::enums::Message::Message(message)) if !message.message.is_empty() => message
            .message
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
        // Empty text covers media, which has no preview worth the round trip.
        Some(tl::enums::Message::Message(_)) => "[media]".to_string(),
        Some(tl::enums::Message::Service(message)) => {
            call_label(&message.action, message.out).unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// Read state as the dialog list reports it.
///
/// `dialogFolder` is the archive row rather than a conversation and carries none of these fields
/// at all, which is why this is a match and not a field access.
fn read_state(raw: &tl::enums::Dialog, ticks: bool) -> (Option<i32>, usize) {
    match raw {
        tl::enums::Dialog::Dialog(raw) => (
            ticks.then_some(raw.read_outbox_max_id),
            raw.unread_count.max(0) as usize,
        ),
        tl::enums::Dialog::Folder(_) => (None, 0),
    }
}

/// Which folder a dialog row says it is in.
///
/// Load-bearing on the *main* path, not just the archive one. `DialogIter` sends
/// `messages.getDialogs` with `folder_id` absent, and an absent flag does not mean folder 0 — it
/// means *every* folder, so the main fetch delivers archived chats mixed in with the rest. The row
/// is the only thing that says which is which, and assuming folder 0 here put archived chats in the
/// "All" tab.
///
/// `dialogFolder` is the archive group rather than a chat in it, so it is not itself archived.
fn is_archived(raw: &tl::enums::Dialog) -> bool {
    match raw {
        tl::enums::Dialog::Dialog(raw) => raw.folder_id == Some(ARCHIVE_FOLDER),
        tl::enums::Dialog::Folder(_) => false,
    }
}

/// Whether a mute deadline is still in force at `now`.
///
/// Muting is a deadline, not a flag: `mute_until` is the unix second the chat becomes noisy again,
/// and clients write a far-future one to mean "forever". `now` is passed in rather than read here
/// so the boundary is testable — and this is shared with the live-update path in
/// [`crate::telegram`], so the seed and the update can never disagree about what the field means.
pub fn is_muted(mute_until: Option<i32>, now: i64) -> bool {
    mute_until.is_some_and(|until| i64::from(until) > now)
}

/// Pin and mute as the dialog list reports them, as `(pinned, muted)`.
///
/// `dialogFolder` carries neither field, for the same reason it carries no read state.
fn notify_state(raw: &tl::enums::Dialog, now: i64) -> (bool, bool) {
    let tl::enums::Dialog::Dialog(raw) = raw else {
        return (false, false);
    };
    let tl::enums::PeerNotifySettings::Settings(settings) = &raw.notify_settings;
    (raw.pinned, is_muted(settings.mute_until, now))
}

/// Which view of the pool is on screen.
///
/// A ring, in the order the tab strip draws it: the main list, then the account's own folders, then
/// the archive. `Custom` indexes [`DialogListState::folders`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FolderTab {
    #[default]
    Main,
    Custom(usize),
    Archive,
}

/// How much of one server folder has been read.
///
/// Two of these, because the main list and the archive are paged independently and reaching the
/// end of one says nothing about the other.
#[derive(Debug, Default)]
pub struct Cursor {
    pub loading: bool,
    /// `true` once the server has no more dialogs to give.
    pub exhausted: bool,
}

#[derive(Debug, Default)]
pub struct DialogListState {
    /// Every dialog loaded so far, from both server folders. Order is the server's.
    pub items: Vec<DialogSummary>,
    /// The account's own folders, in the order the Telegram app shows them.
    pub folders: Vec<Folder>,
    pub tab: FolderTab,
    /// Index into the *visible* list, not into `items` — a filtered tab shows a subset, and this
    /// is handed straight to ratatui. Kept on the same conversation across every mutation by
    /// [`DialogListState::keeping_selection`].
    pub selected: usize,
    pub main: Cursor,
    pub archive: Cursor,
}

impl DialogListState {
    /// The pool indices the active tab shows, in pool order.
    ///
    /// Recomputed on demand rather than cached: `ui::draw` rebuilds every frame from scratch
    /// anyway, and a stale membership after a mute or an unread change would be a filter lying
    /// about what it contains.
    pub fn visible(&self) -> Vec<usize> {
        (0..self.items.len())
            .filter(|&index| self.shows(&self.items[index]))
            .collect()
    }

    fn shows(&self, item: &DialogSummary) -> bool {
        match self.tab {
            FolderTab::Main => !item.archived,
            FolderTab::Archive => item.archived,
            FolderTab::Custom(index) => self
                .folders
                .get(index)
                .is_some_and(|folder| folders::matches(&folder.rule, item)),
        }
    }

    /// The tabs in strip order, as titles. Always at least "All" and "Archive".
    pub fn tabs(&self) -> Vec<(FolderTab, String)> {
        let mut tabs = vec![(FolderTab::Main, "All".to_string())];
        tabs.extend(
            self.folders
                .iter()
                .enumerate()
                .map(|(index, folder)| (FolderTab::Custom(index), folder.title.clone())),
        );
        tabs.push((FolderTab::Archive, "Archive".to_string()));
        tabs
    }

    /// Where the active tab sits in [`DialogListState::tabs`].
    pub fn tab_index(&self) -> usize {
        match self.tab {
            FolderTab::Main => 0,
            FolderTab::Custom(index) => index + 1,
            FolderTab::Archive => self.folders.len() + 1,
        }
    }

    /// Step to the next or previous tab, wrapping.
    ///
    /// Wrapping, unlike the picture viewer's clamped `←`/`→`: the strip is a ring whose ends are
    /// both on screen, so stepping off one and arriving at the other reads as movement rather than
    /// as a lost keystroke.
    pub fn step_tab(&mut self, forward: bool) {
        let tabs = self.tabs();
        let count = tabs.len();
        let next = if forward {
            (self.tab_index() + 1) % count
        } else {
            (self.tab_index() + count - 1) % count
        };
        self.tab = tabs[next].0;
        // A different tab is a different list; carrying an index over would land the highlight on
        // an unrelated conversation and open it.
        self.selected = 0;
    }

    /// Replace the account's folders after `messages.getDialogFilters`.
    pub fn set_folders(&mut self, folders: Vec<Folder>) {
        self.keeping_selection(|state| {
            state.folders = folders;
            // A folder deleted on another device must not leave the strip pointing past its end.
            if let FolderTab::Custom(index) = state.tab
                && index >= state.folders.len()
            {
                state.tab = FolderTab::Main;
            }
        });
    }

    /// The cursor for the server folder the active tab reads from.
    ///
    /// A custom folder is a filter over the main list, so it pages the main list — which is why
    /// this is a two-way split and not three.
    pub fn cursor(&self) -> &Cursor {
        match self.tab {
            FolderTab::Archive => &self.archive,
            _ => &self.main,
        }
    }

    pub fn cursor_mut(&mut self) -> &mut Cursor {
        match self.tab {
            FolderTab::Archive => &mut self.archive,
            _ => &mut self.main,
        }
    }

    /// Whether the active tab reads the archive rather than the main list.
    pub fn showing_archive(&self) -> bool {
        self.tab == FolderTab::Archive
    }

    pub fn selected_peer(&self) -> Option<PeerRef> {
        self.selected_summary().map(|item| item.peer)
    }

    pub fn selected_summary(&self) -> Option<&DialogSummary> {
        let index = *self.visible().get(self.selected)?;
        self.items.get(index)
    }

    pub fn find(&self, peer_id: PeerId) -> Option<&DialogSummary> {
        self.items.iter().find(|item| item.peer.id == peer_id)
    }

    /// Fold a page into the pool.
    ///
    /// Deduped by peer, because the two cursors overlap: the main fetch asks for every folder at
    /// once (see [`is_archived`]), so an archived chat can arrive from it *and* from the archive's
    /// own paging. The row already held wins — it is the one carrying whatever this session has
    /// since done to it, and a second copy would show as a duplicate row and take a `j` of its own
    /// to scroll past.
    pub fn extend(&mut self, items: Vec<DialogSummary>, exhausted: bool, archived: bool) {
        let cursor = if archived {
            &mut self.archive
        } else {
            &mut self.main
        };
        cursor.loading = false;
        cursor.exhausted = exhausted;

        self.keeping_selection(|state| {
            for item in items {
                match state.index_of(item.peer.id) {
                    // Not new, but the server has just restated which folder it is in — and that
                    // may be news, since nothing else tells us about a chat archived elsewhere
                    // while this client was not running.
                    Some(index) => state.items[index].archived = item.archived,
                    None => state.items.push(item),
                }
            }
        });
    }

    pub fn select_next(&mut self) {
        if self.selected + 1 < self.visible().len() {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Whether scrolling has come close enough to the end to warrant fetching another page.
    ///
    /// Measured against the *visible* list. On a custom folder that means a sparse one keeps
    /// pulling pages of the main list until it has rows to show or the account runs out — the
    /// price of folders being filters Telegram evaluates client-side rather than a query.
    pub fn wants_more(&self) -> bool {
        let cursor = self.cursor();
        !cursor.exhausted
            && !cursor.loading
            && self.selected + PREFETCH_MARGIN >= self.visible().len()
    }

    /// Move an existing dialog to the top and refresh its preview after a new message.
    pub fn bump(&mut self, peer_id: PeerId, preview: String) {
        let Some(index) = self.index_of(peer_id) else {
            return;
        };
        self.items[index].preview = preview;
        self.move_to_top(index);
    }

    /// Move a chat between the main list and the archive, after the server confirmed it.
    ///
    /// The row stays in the pool: the conversation still exists, it is just in the other folder
    /// now, and dropping it would throw away everything known about it — including, for a chat
    /// archived from a phone, a summary the archive tab has not paged in yet.
    pub fn set_archived(&mut self, peer_id: PeerId, archived: bool) {
        self.keeping_selection(|state| {
            if let Some(item) = state.find_mut(peer_id) {
                item.archived = archived;
            }
        });
    }

    /// Run a mutation and leave the highlight on whichever conversation it was on.
    ///
    /// `selected` indexes the visible list, so nothing can adjust it arithmetically the way a
    /// single flat list could: a row can leave the view by being removed, by being reordered, by
    /// moving to the other folder, or by a folder rule changing under it. Anchoring on the peer
    /// covers all four, and falling back to the old position is what hands the highlight to
    /// whatever moved up into the vacated slot — including when that slot was the last one.
    fn keeping_selection<R>(&mut self, change: impl FnOnce(&mut Self) -> R) -> R {
        let anchor = self.selected_peer().map(|peer| peer.id);
        let position = self.selected;

        let result = change(self);

        let visible = self.visible();
        self.selected = anchor
            .and_then(|id| {
                visible
                    .iter()
                    .position(|&index| self.items[index].peer.id == id)
            })
            .unwrap_or_else(|| position.min(visible.len().saturating_sub(1)));
        result
    }

    /// Blank a chat's preview line, because its history was just emptied.
    pub fn clear_preview(&mut self, peer_id: PeerId) {
        if let Some(item) = self.find_mut(peer_id) {
            item.preview.clear();
        }
    }

    /// Silence or unsilence a chat, after the server has confirmed it.
    pub fn set_muted(&mut self, peer_id: PeerId, muted: bool) {
        if let Some(item) = self.find_mut(peer_id) {
            item.muted = muted;
        }
    }

    /// Pin or unpin a chat.
    ///
    /// Pinning also moves the row to the top, which is where the server would put it on the next
    /// `messages.getDialogs`. Unpinning leaves the row where it is rather than guessing which of
    /// the chats below it now outranks it — the next start reads the true order back.
    pub fn set_pinned(&mut self, peer_id: PeerId, pinned: bool) {
        let Some(index) = self.index_of(peer_id) else {
            return;
        };
        self.items[index].pinned = pinned;
        if pinned {
            self.move_to_top(index);
        }
    }

    /// Record that a user is (or is no longer) on the account's blocked list.
    pub fn set_blocked(&mut self, peer_id: PeerId, blocked: bool) {
        if let Some(item) = self.find_mut(peer_id) {
            item.blocked = blocked;
        }
    }

    /// Seed blocked state from `contacts.getBlocked`, which answers for the whole account at once.
    ///
    /// Assigned rather than or-ed: this *is* the list, so a peer missing from it is not blocked.
    pub fn set_blocked_list(&mut self, blocked: &[PeerId]) {
        for item in &mut self.items {
            item.blocked = blocked.contains(&item.peer.id);
        }
    }

    /// Drop a conversation that is no longer in the list — left, deleted, or archived away.
    ///
    /// Returns whether a row actually went, because the caller has to close the chat pane if it
    /// was the open one.
    pub fn remove(&mut self, peer_id: PeerId) -> bool {
        self.keeping_selection(|state| {
            let Some(index) = state.index_of(peer_id) else {
                return false;
            };
            state.items.remove(index);
            true
        })
    }

    fn index_of(&self, peer_id: PeerId) -> Option<usize> {
        self.items.iter().position(|item| item.peer.id == peer_id)
    }

    /// Move a row to the front, keeping the highlight on the conversation it was already on.
    fn move_to_top(&mut self, index: usize) {
        self.keeping_selection(|state| {
            let item = state.items.remove(index);
            state.items.insert(0, item);
        });
    }

    /// Raise the watermark after the other side reported reading up to `max_id`.
    pub fn mark_outbox_read(&mut self, peer_id: PeerId, max_id: i32) {
        let Some(item) = self.find_mut(peer_id) else {
            return;
        };
        // `None` means receipts are meaningless in this chat, and a watermark must not switch them
        // on. Where they do apply the mark only ever moves forwards — resolving an update gap can
        // replay an older one after a newer one.
        if let Some(current) = item.read_outbox_max_id.as_mut() {
            *current = (*current).max(max_id);
        }
    }

    /// Count one more incoming message the user hasn't seen.
    pub fn mark_unread(&mut self, peer_id: PeerId) {
        if let Some(item) = self.find_mut(peer_id) {
            item.unread += 1;
        }
    }

    /// Forget the badge because the user just opened the chat.
    ///
    /// Local only, and deliberately so — nothing is sent to Telegram, so the conversation stays
    /// unread everywhere else.
    pub fn clear_unread(&mut self, peer_id: PeerId) {
        if let Some(item) = self.find_mut(peer_id) {
            item.unread = 0;
        }
    }

    /// Fold in the server's own count after the user read the chat on another device.
    ///
    /// Clamped rather than assigned. Because tgtui never acknowledges a read, the server's count
    /// still includes messages already seen here — taking it verbatim would resurrect a badge the
    /// user had cleared. It can only ever prove the count is *lower* than we thought, which is
    /// exactly what `min` takes from it.
    pub fn reconcile_unread(&mut self, peer_id: PeerId, still_unread: usize) {
        if let Some(item) = self.find_mut(peer_id) {
            item.unread = item.unread.min(still_unread);
        }
    }

    fn find_mut(&mut self, peer_id: PeerId) -> Option<&mut DialogSummary> {
        self.items.iter_mut().find(|item| item.peer.id == peer_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_session::types::PeerAuth;

    use crate::test_support::{
        archived_dialog, dialog, folder, raw_channel, raw_dialog, raw_dialog_with, raw_folder,
        raw_message, raw_user,
    };

    fn list(count: i64) -> DialogListState {
        DialogListState {
            items: (1..=count)
                .map(|id| dialog(id, &format!("chat {id}")))
                .collect(),
            ..Default::default()
        }
    }

    /// The names the active tab shows, in the order it shows them.
    fn names(state: &DialogListState) -> Vec<&str> {
        state
            .visible()
            .into_iter()
            .map(|index| state.items[index].name.as_str())
            .collect()
    }

    /// Whichever conversation was highlighted must stay highlighted after a reorder.
    fn assert_selection_follows(selected: usize, bumped: usize) {
        let mut state = list(5);
        state.selected = selected;
        let expected = state.items[selected].name.clone();
        let bumped_peer = state.items[bumped].peer.id;

        state.bump(bumped_peer, "new".to_string());

        assert_eq!(
            state.items[state.selected].name, expected,
            "selection drifted when row {bumped} was bumped with row {selected} selected"
        );
    }

    #[test]
    fn bump_moves_the_chat_to_the_top_with_a_fresh_preview() {
        let mut state = list(3);
        let peer = state.items[2].peer.id;

        state.bump(peer, "newest line".to_string());

        assert_eq!(state.items[0].name, "chat 3");
        assert_eq!(state.items[0].preview, "newest line");
        assert_eq!(
            state
                .items
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            ["chat 3", "chat 1", "chat 2"]
        );
    }

    #[test]
    fn selection_follows_its_chat_through_a_bump() {
        // The three cases that matter: the bumped row is below, above, or is the selection.
        assert_selection_follows(1, 3);
        assert_selection_follows(3, 1);
        assert_selection_follows(2, 2);
    }

    #[test]
    fn bumping_an_unknown_chat_changes_nothing() {
        let mut state = list(2);
        let before: Vec<_> = state.items.iter().map(|i| i.name.clone()).collect();

        state.bump(dialog(99, "stranger").peer.id, "hi".to_string());

        assert_eq!(
            state
                .items
                .iter()
                .map(|i| i.name.clone())
                .collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn more_dialogs_are_wanted_only_near_the_end_of_the_list() {
        let mut state = list(20);

        state.selected = 0;
        assert!(!state.wants_more());

        state.selected = state.items.len() - PREFETCH_MARGIN;
        assert!(state.wants_more());

        state.main.loading = true;
        assert!(!state.wants_more(), "must not stack requests while loading");

        state.main.loading = false;
        state.main.exhausted = true;
        assert!(!state.wants_more(), "must stop once the server is drained");
    }

    /// The bug this pins: `messages.getDialogs` with the `folder_id` flag *absent* means every
    /// folder, not folder 0, so the main fetch delivers archived chats and only the row itself says
    /// so. Reading it wrong put them in the "All" tab.
    #[test]
    fn the_main_fetch_files_its_archived_rows_under_the_archive_tab() {
        let archived = tl::enums::Dialog::Dialog(tl::types::Dialog {
            folder_id: Some(1),
            ..match raw_dialog(0, 0) {
                tl::enums::Dialog::Dialog(dialog) => dialog,
                tl::enums::Dialog::Folder(_) => unreachable!(),
            }
        });

        assert!(is_archived(&archived));
        assert!(
            !is_archived(&raw_dialog(0, 0)),
            "no folder on the row is the main list"
        );
        assert!(
            !is_archived(&raw_folder()),
            "`dialogFolder` is the archive group itself, not a chat inside it"
        );
    }

    #[test]
    fn a_chat_both_cursors_deliver_is_held_once() {
        let mut state = DialogListState::default();

        state.extend(vec![archived_dialog(1, "Old friend")], false, false);
        state.extend(vec![archived_dialog(1, "Old friend")], true, true);

        assert_eq!(
            state.items.len(),
            1,
            "the main fetch covers every folder, so the archive's own paging restates rows it \
             already delivered"
        );
    }

    #[test]
    fn a_restated_row_brings_its_folder_up_to_date() {
        let mut state = list(1);
        let peer = state.items[0].peer.id;

        state.extend(vec![archived_dialog(1, "chat 1")], false, false);

        assert_eq!(state.items.len(), 1);
        assert!(
            state.find(peer).unwrap().archived,
            "a chat archived from a phone while this client was off is only reported by the row"
        );
    }

    // -- folder tabs ---------------------------------------------------------

    /// Both server folders live in one `items`, so every tab is a filter and none of them owns a
    /// list of its own.
    #[test]
    fn each_tab_shows_its_own_slice_of_the_one_pool() {
        let mut state = list(2);
        state.items.push(archived_dialog(9, "old friend"));
        let work = state.items[0].peer.id;
        state.folders = vec![folder("Work", &[work])];

        assert_eq!(names(&state), ["chat 1", "chat 2"]);

        state.tab = FolderTab::Custom(0);
        assert_eq!(names(&state), ["chat 1"]);

        state.tab = FolderTab::Archive;
        assert_eq!(
            names(&state),
            ["old friend"],
            "an archived chat belongs to the archive tab and to no other view by default"
        );
    }

    #[test]
    fn stepping_through_the_tabs_wraps_at_both_ends() {
        let mut state = list(1);
        state.folders = vec![folder("Work", &[]), folder("Personal", &[])];

        let mut seen = vec![state.tab];
        for _ in 0..3 {
            state.step_tab(true);
            seen.push(state.tab);
        }
        assert_eq!(
            seen,
            [
                FolderTab::Main,
                FolderTab::Custom(0),
                FolderTab::Custom(1),
                FolderTab::Archive
            ]
        );

        state.step_tab(true);
        assert_eq!(state.tab, FolderTab::Main, "the strip is a ring");
        state.step_tab(false);
        assert_eq!(state.tab, FolderTab::Archive, "and it turns both ways");
    }

    #[test]
    fn a_tab_switch_starts_the_selection_at_the_top_of_the_new_list() {
        let mut state = list(5);
        state.selected = 4;

        state.step_tab(true);

        assert_eq!(
            state.selected, 0,
            "carrying the index over would highlight — and open — an unrelated conversation"
        );
    }

    #[test]
    fn a_folder_deleted_elsewhere_does_not_leave_the_strip_pointing_past_its_end() {
        let mut state = list(1);
        state.folders = vec![folder("Work", &[]), folder("Personal", &[])];
        state.tab = FolderTab::Custom(1);

        state.set_folders(vec![folder("Work", &[])]);

        assert_eq!(state.tab, FolderTab::Main);
    }

    #[test]
    fn the_archive_tab_pages_the_archive_and_every_other_tab_pages_the_main_list() {
        let mut state = list(1);
        state.main.exhausted = true;
        state.archive.exhausted = false;

        assert!(!state.wants_more());

        state.tab = FolderTab::Archive;
        assert!(
            state.wants_more(),
            "the archive is a separate cursor; draining the main list says nothing about it"
        );
    }

    /// A folder is a filter over the main list, so it has to keep pulling pages until it has rows
    /// of its own — the server will not answer "the chats in Work".
    #[test]
    fn a_folder_with_nothing_in_it_yet_keeps_asking_for_more_of_the_main_list() {
        let mut state = list(30);
        state.folders = vec![folder("Work", &[])];
        state.tab = FolderTab::Custom(0);

        assert!(state.visible().is_empty());
        assert!(state.wants_more());
    }

    // -- the archive parser --------------------------------------------------

    /// The peer of the row `raw_dialog` builds, which names user 1.
    fn user_peer() -> tl::enums::Peer {
        tl::enums::Peer::User(tl::types::PeerUser { user_id: 1 })
    }

    #[test]
    fn an_archived_row_is_built_from_the_wire_with_everything_the_list_draws() {
        let summary = DialogSummary::from_raw(
            &raw_dialog(42, 3),
            &[raw_user(1, "Alice", true, false)],
            &[],
            &[raw_message(
                user_peer(),
                42,
                "see you then\nand bring the map",
            )],
        )
        .expect("a private chat with its peer in the response resolves");

        assert_eq!(summary.name, "Alice");
        assert_eq!(
            summary.preview, "see you then",
            "the preview is one line, whatever the message is"
        );
        assert_eq!(summary.read_outbox_max_id, Some(42));
        assert_eq!(summary.unread, 3);
        assert!(summary.contact && !summary.bot);
        assert!(
            summary.archived,
            "this path exists only for folder 1, so nothing it builds belongs in the main list"
        );
        assert_eq!(
            summary.peer.auth,
            PeerAuth::from_hash(1),
            "the access hash off the response is what opens and sends to an archived chat — \
             nothing else has cached this peer"
        );
    }

    #[test]
    fn an_archived_megagroup_is_a_group_rather_than_a_channel() {
        let raw = tl::enums::Dialog::Dialog(tl::types::Dialog {
            peer: tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 5 }),
            ..match raw_dialog(0, 0) {
                tl::enums::Dialog::Dialog(dialog) => dialog,
                tl::enums::Dialog::Folder(_) => unreachable!(),
            }
        });

        let group = DialogSummary::from_raw(&raw, &[], &[raw_channel(5, "Rust Users", true)], &[])
            .expect("a channel-shaped peer in the response resolves");
        assert_eq!(group.kind, DialogKind::Group { megagroup: true });

        let channel =
            DialogSummary::from_raw(&raw, &[], &[raw_channel(5, "Rust Blog", false)], &[])
                .expect("a broadcast channel resolves the same way");
        assert_eq!(channel.kind, DialogKind::Channel);
        assert_eq!(
            channel.read_outbox_max_id, None,
            "a broadcast channel has readers rather than a recipient, on this path too"
        );
    }

    #[test]
    fn a_row_whose_peer_the_response_left_out_is_dropped_rather_than_guessed() {
        assert!(
            DialogSummary::from_raw(&raw_dialog(1, 0), &[], &[], &[]).is_none(),
            "without the access hash there is no `PeerRef`, so the row could never be opened"
        );
    }

    #[test]
    fn the_archive_folder_row_is_not_a_conversation() {
        assert!(
            DialogSummary::from_raw(&raw_folder(), &[], &[], &[]).is_none(),
            "`dialogFolder` stands for a group of chats and has none of the fields a row needs"
        );
    }

    /// Message ids restart at 1 in every channel, so a response holding several chats has several
    /// messages that could answer to the same `top_message`.
    #[test]
    fn a_preview_comes_from_the_right_chats_message_and_not_just_the_right_id() {
        let summary = DialogSummary::from_raw(
            &raw_dialog(7, 0),
            &[raw_user(1, "Alice", false, false)],
            &[raw_channel(5, "Rust Blog", false)],
            &[
                raw_message(
                    tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 5 }),
                    7,
                    "someone else's line",
                ),
                raw_message(user_peer(), 7, "ours"),
            ],
        )
        .unwrap();

        assert_eq!(summary.preview, "ours");
    }

    // -- archiving -----------------------------------------------------------

    #[test]
    fn archiving_moves_a_chat_between_tabs_instead_of_dropping_it() {
        let mut state = list(2);
        let peer = state.items[0].peer.id;

        state.set_archived(peer, true);

        assert_eq!(names(&state), ["chat 2"]);
        state.tab = FolderTab::Archive;
        assert_eq!(names(&state), ["chat 1"]);

        state.set_archived(peer, false);
        assert!(
            state.visible().is_empty(),
            "unarchiving must take the row back out of the archive"
        );
        assert_eq!(
            state.items.len(),
            2,
            "the conversation still exists; only its folder changed"
        );
    }

    #[test]
    fn a_chat_archived_from_under_the_selection_hands_the_highlight_on() {
        let mut state = list(3);
        state.selected = 1;

        state.set_archived(state.items[1].peer.id, true);

        assert_eq!(
            state.items[state.visible()[state.selected]].name,
            "chat 3",
            "the row that moved up into the vacated slot is the natural next selection"
        );
    }

    #[test]
    fn the_highlight_stays_on_its_chat_when_a_row_above_it_is_archived() {
        let mut state = list(4);
        state.selected = 2;

        state.set_archived(state.items[0].peer.id, true);

        assert_eq!(state.items[state.visible()[state.selected]].name, "chat 3");
    }

    // -- read state ----------------------------------------------------------

    #[test]
    fn a_dialog_seeds_its_watermark_and_unread_count_from_the_server() {
        assert_eq!(read_state(&raw_dialog(42, 7), true), (Some(42), 7));
    }

    #[test]
    fn an_archive_folder_row_carries_no_read_state_at_all() {
        // `dialogFolder` has none of these fields, so reading them off it would not compile —
        // this pins that the match keeps a separate arm for it rather than assuming a `dialog`.
        assert_eq!(read_state(&raw_folder(), true), (None, 0));
    }

    #[test]
    fn a_chat_where_a_receipt_means_nothing_gets_no_watermark_to_begin_with() {
        let (watermark, unread) = read_state(&raw_dialog(42, 7), false);
        assert_eq!(
            watermark, None,
            "a broadcast channel's outbox watermark is not worth believing, and `None` is what \
             stops the transcript reserving a column for a tick it will never draw"
        );
        assert_eq!(unread, 7, "suppressing ticks must not suppress the badge");
    }

    #[test]
    fn the_outbox_watermark_only_ever_moves_forwards() {
        let mut state = list(2);
        let peer = state.items[0].peer.id;

        state.mark_outbox_read(peer, 50);
        state.mark_outbox_read(peer, 30);

        assert_eq!(
            state.items[0].read_outbox_max_id,
            Some(50),
            "resolving an update gap can replay an older read after a newer one, and a ✓✓ that \
             fell back to ✓ would read as the message having been un-read"
        );
    }

    #[test]
    fn a_chat_with_receipts_suppressed_ignores_a_read_watermark() {
        let mut state = list(1);
        state.items[0].read_outbox_max_id = None;
        let peer = state.items[0].peer.id;

        state.mark_outbox_read(peer, 50);

        assert_eq!(
            state.items[0].read_outbox_max_id, None,
            "an update must not switch on ticks the chat was deliberately built without"
        );
    }

    #[test]
    fn reading_on_another_device_never_raises_a_badge_the_user_already_cleared() {
        let mut state = list(1);
        let peer = state.items[0].peer.id;
        state.clear_unread(peer);

        state.reconcile_unread(peer, 4);

        assert_eq!(
            state.items[0].unread, 0,
            "the server still counts messages already read here, because tgtui never sends an \
             acknowledgement — taking its number verbatim would resurrect a cleared badge"
        );
    }

    #[test]
    fn reading_on_another_device_lowers_a_badge_it_can_prove_is_stale() {
        let mut state = list(1);
        let peer = state.items[0].peer.id;
        state.items[0].unread = 5;

        state.reconcile_unread(peer, 2);

        assert_eq!(state.items[0].unread, 2);
    }

    #[test]
    fn bumping_a_chat_does_not_touch_its_unread_count() {
        let mut state = list(2);
        let peer = state.items[1].peer.id;
        state.items[1].unread = 3;

        // `bump` also runs for messages we sent, where raising a badge would be a bug.
        state.bump(peer, "newest".to_string());

        assert_eq!(state.items[0].unread, 3);
    }

    // -- mute, pin, block ----------------------------------------------------

    /// A mute is a deadline, so "muted" depends on the clock, not just on the field being set.
    #[test]
    fn a_mute_deadline_in_the_past_is_not_a_mute() {
        let now = 1_700_000_000;

        let (_, muted) = notify_state(&raw_dialog_with(0, 0, false, Some(1_699_999_999)), now);
        assert!(
            !muted,
            "the mute expired a second ago; the chat is noisy again"
        );

        let (_, muted) = notify_state(&raw_dialog_with(0, 0, false, Some(1_700_000_001)), now);
        assert!(muted);

        let (_, muted) = notify_state(&raw_dialog_with(0, 0, false, None), now);
        assert!(!muted, "no deadline at all means the chat was never muted");
    }

    /// Clients write a far-future second to mean "forever", which must not overflow the compare.
    #[test]
    fn a_mute_forever_reads_as_muted_rather_than_wrapping() {
        let (_, muted) = notify_state(
            &raw_dialog_with(0, 0, false, Some(i32::MAX)),
            i64::from(i32::MAX) - 1,
        );
        assert!(muted);
    }

    #[test]
    fn the_pin_flag_is_carried_straight_off_the_dialog_row() {
        assert!(notify_state(&raw_dialog_with(0, 0, true, None), 0).0);
        assert!(!notify_state(&raw_dialog(0, 0), 0).0);
    }

    #[test]
    fn an_archive_folder_row_carries_no_notify_state_either() {
        assert_eq!(
            notify_state(&raw_folder(), 0),
            (false, false),
            "`dialogFolder` has neither field, so this must stay a match rather than a field access"
        );
    }

    #[test]
    fn pinning_lifts_the_chat_to_where_the_server_would_put_it() {
        let mut state = list(3);
        let peer = state.items[2].peer.id;

        state.set_pinned(peer, true);

        assert_eq!(state.items[0].name, "chat 3");
        assert!(state.items[0].pinned);
    }

    #[test]
    fn unpinning_marks_the_chat_without_guessing_a_new_position() {
        let mut state = list(3);
        let peer = state.items[0].peer.id;
        state.set_pinned(peer, true);

        state.set_pinned(peer, false);

        assert!(!state.items[0].pinned);
        assert_eq!(
            state.items[0].name, "chat 1",
            "there is no way to know which chat now outranks it, and the next start reads the \
             true order back anyway"
        );
    }

    #[test]
    fn the_blocked_list_is_the_whole_truth_and_clears_peers_missing_from_it() {
        let mut state = list(3);
        let first = state.items[0].peer.id;
        let second = state.items[1].peer.id;
        state.set_blocked(first, true);
        state.set_blocked(second, true);

        state.set_blocked_list(&[second]);

        assert!(
            !state.items[0].blocked && state.items[1].blocked,
            "`contacts.getBlocked` answers for the whole account, so a peer it omits is not blocked"
        );
    }

    // -- removal -------------------------------------------------------------

    #[test]
    fn removing_a_row_above_the_selection_keeps_the_same_chat_highlighted() {
        let mut state = list(5);
        state.selected = 3;
        let expected = state.items[3].name.clone();

        state.remove(state.items[1].peer.id);

        assert_eq!(state.items[state.selected].name, expected);
    }

    #[test]
    fn removing_a_row_below_the_selection_leaves_the_index_alone() {
        let mut state = list(5);
        state.selected = 1;

        state.remove(state.items[4].peer.id);

        assert_eq!(state.items[state.selected].name, "chat 2");
    }

    #[test]
    fn removing_the_selected_row_hands_the_highlight_to_the_one_below() {
        let mut state = list(5);
        state.selected = 2;

        state.remove(state.items[2].peer.id);

        assert_eq!(
            state.items[state.selected].name, "chat 4",
            "the row that moved up into the vacated slot is the natural next selection"
        );
    }

    /// The one case that would panic on the next render: `selected` is a bare index, and ratatui
    /// is handed it verbatim.
    #[test]
    fn removing_the_last_row_pulls_the_selection_back_into_bounds() {
        let mut state = list(3);
        state.selected = 2;

        state.remove(state.items[2].peer.id);

        assert_eq!(state.selected, 1);
        assert!(state.items.get(state.selected).is_some());
    }

    #[test]
    fn removing_the_only_row_leaves_an_empty_list_with_a_selection_of_zero() {
        let mut state = list(1);

        assert!(state.remove(state.items[0].peer.id));

        assert!(state.items.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn removing_a_chat_that_is_not_in_the_list_reports_that_nothing_went() {
        let mut state = list(2);

        assert!(
            !state.remove(dialog(99, "stranger").peer.id),
            "the caller closes the open chat on a `true`, so a miss must not claim a removal"
        );
        assert_eq!(state.items.len(), 2);
    }

    #[test]
    fn read_state_for_an_unknown_dialog_is_ignored() {
        let mut state = list(2);
        let stranger = dialog(99, "stranger").peer.id;

        state.mark_outbox_read(stranger, 10);
        state.mark_unread(stranger);
        state.clear_unread(stranger);
        state.reconcile_unread(stranger, 0);

        assert!(
            state
                .items
                .iter()
                .all(|item| item.unread == 0 && item.read_outbox_max_id == Some(0)),
            "an update for a chat we have not loaded must not land on a loaded one"
        );
    }
}
