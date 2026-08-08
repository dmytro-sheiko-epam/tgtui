//! The chat list: dialog summaries plus the cursor for lazily loading more of them.

use grammers_client::peer::Dialog;
use grammers_session::types::{PeerId, PeerRef};

/// How many dialogs to pull per page.
pub const PAGE_SIZE: usize = 30;

/// Start loading more dialogs once the selection is this close to the end of the loaded list.
const PREFETCH_MARGIN: usize = 5;

#[derive(Debug, Clone)]
pub struct DialogSummary {
    pub peer: PeerRef,
    pub name: String,
    pub preview: String,
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
                    "[media]".to_string()
                } else {
                    text.lines().next().unwrap_or_default().to_string()
                }
            })
            .unwrap_or_default();

        Self {
            peer: dialog.peer_ref(),
            name,
            preview,
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::dialog;

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
}
