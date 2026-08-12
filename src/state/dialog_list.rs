//! The chat list: dialog summaries plus the cursor for lazily loading more of them.

use chrono::Utc;
use grammers_client::peer::Dialog;
use grammers_client::tl;
use grammers_session::types::{PeerId, PeerRef};

use crate::state::call::call_label;
use crate::state::dialog_actions::DialogKind;

/// How many dialogs to pull per page.
pub const PAGE_SIZE: usize = 30;

/// Start loading more dialogs once the selection is this close to the end of the loaded list.
const PREFETCH_MARGIN: usize = 5;

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

    pub fn selected_summary(&self) -> Option<&DialogSummary> {
        self.items.get(self.selected)
    }

    pub fn find(&self, peer_id: PeerId) -> Option<&DialogSummary> {
        self.items.iter().find(|item| item.peer.id == peer_id)
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
        let Some(index) = self.index_of(peer_id) else {
            return;
        };
        self.items[index].preview = preview;
        self.move_to_top(index);
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
        let Some(index) = self.index_of(peer_id) else {
            return false;
        };
        self.items.remove(index);

        // `selected` is a bare index, so a removal above it shifts the wrong row under the
        // highlight, and removing the last row leaves it out of bounds entirely. Rows below the
        // selection need no adjustment; the row *at* the selection hands the highlight to whatever
        // moved up into its place.
        if self.selected > index {
            self.selected -= 1;
        }
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        true
    }

    fn index_of(&self, peer_id: PeerId) -> Option<usize> {
        self.items.iter().position(|item| item.peer.id == peer_id)
    }

    /// Move a row to the front, keeping the highlight on the conversation it was already on.
    fn move_to_top(&mut self, index: usize) {
        let item = self.items.remove(index);
        self.items.insert(0, item);

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
    use crate::test_support::{dialog, raw_dialog, raw_dialog_with, raw_folder};

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
