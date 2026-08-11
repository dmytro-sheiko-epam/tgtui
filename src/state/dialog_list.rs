//! The chat list: dialog summaries plus the cursor for lazily loading more of them.

use grammers_client::peer::{Dialog, Peer};
use grammers_client::tl;
use grammers_session::types::{PeerId, PeerRef};

use crate::state::call::call_label;

/// How many dialogs to pull per page.
pub const PAGE_SIZE: usize = 30;

/// Start loading more dialogs once the selection is this close to the end of the loaded list.
const PREFETCH_MARGIN: usize = 5;

#[derive(Debug, Clone)]
pub struct DialogSummary {
    pub peer: PeerRef,
    pub name: String,
    pub preview: String,
    /// Highest id of ours the other side has read, or `None` where a receipt would mean nothing —
    /// see `receipts_make_sense`. Telegram never reports a per-message read flag, only this
    /// per-chat watermark, so it is the whole basis for the ✓✓ in the transcript.
    pub read_outbox_max_id: Option<i32>,
    /// Incoming messages not yet read. Seeded from the server, then maintained locally: tgtui
    /// never acknowledges a read, so this counts from the last time *this* client looked.
    pub unread: usize,
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

        let (read_outbox_max_id, unread) =
            read_state(&dialog.raw, receipts_make_sense(dialog.peer()));

        Self {
            peer: dialog.peer_ref(),
            name,
            preview,
            read_outbox_max_id,
            unread,
        }
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

/// Whether "the other side read this" is a statement worth making.
///
/// A broadcast channel has readers, not a recipient — Telegram reports view counts there and never
/// moves an outbox watermark worth believing. Saved Messages is you at both ends. grammers already
/// splits broadcasts from megagroups, so supergroups keep their ticks, which is right: they do get
/// `updateReadChannelOutbox`.
fn receipts_make_sense(peer: &Peer) -> bool {
    match peer {
        Peer::Channel(_) => false,
        Peer::User(user) => !user.is_self(),
        Peer::Group(_) => true,
    }
}

#[derive(Debug, Default)]
pub struct DialogListState {
    pub items: Vec<DialogSummary>,
    pub selected: usize,
    /// `true` once the server has no more dialogs to give.
    pub exhausted: bool,
    pub loading: bool,
}

impl DialogListState {
    pub fn selected_peer(&self) -> Option<PeerRef> {
        self.items.get(self.selected).map(|item| item.peer)
    }

    pub fn extend(&mut self, items: Vec<DialogSummary>, exhausted: bool) {
        self.loading = false;
        self.exhausted = exhausted;
        self.items.extend(items);
    }

    pub fn select_next(&mut self) {
        if !self.items.is_empty() && self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Whether scrolling has come close enough to the end to warrant fetching another page.
    pub fn wants_more(&self) -> bool {
        !self.exhausted && !self.loading && self.selected + PREFETCH_MARGIN >= self.items.len()
    }

    /// Move an existing dialog to the top and refresh its preview after a new message.
    pub fn bump(&mut self, peer_id: PeerId, preview: String) {
        let Some(index) = self.items.iter().position(|item| item.peer.id == peer_id) else {
            return;
        };
        let mut item = self.items.remove(index);
        item.preview = preview;
        self.items.insert(0, item);

        // Keep the highlighted row pointing at the same conversation it was on before the move.
        if self.selected == index {
            self.selected = 0;
        } else if self.selected < index {
            self.selected += 1;
        }
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
    use crate::test_support::{dialog, raw_dialog, raw_folder};

    fn list(count: i64) -> DialogListState {
        DialogListState {
            items: (1..=count)
                .map(|id| dialog(id, &format!("chat {id}")))
                .collect(),
            ..Default::default()
        }
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

        state.loading = true;
        assert!(!state.wants_more(), "must not stack requests while loading");

        state.loading = false;
        state.exhausted = true;
        assert!(!state.wants_more(), "must stop once the server is drained");
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
