//! The Telegram actor: the only part of the app that talks to grammers.
//!
//! The UI never awaits a network call. It pushes a [`TgCommand`] down a channel and later
//! receives a [`TgEvent`] back, which keeps the render loop responsive while requests are in
//! flight.

pub mod commands;
pub mod events;
pub mod session;

use std::future::Future;
use std::sync::Arc;

use chrono::Utc;
use color_eyre::eyre::{Result, eyre};
use grammers_client::client::{DialogIter, LoginToken, PasswordToken, UpdatesConfiguration};
use grammers_client::message::InputMessage;
use grammers_client::tl;
use grammers_client::update::Update;
use grammers_client::{Client, InvocationError, SenderPool, SignInError};
use grammers_session::types::{PeerId, PeerKind, PeerRef};
use grammers_session::updates::UpdatesLike;
use image::DynamicImage;
use tokio::sync::{Mutex, mpsc};

pub use commands::TgCommand;
pub use events::TgEvent;

use crate::config::Config;
use crate::state::chat_buffer::{self, ChatMessage, ReplyPreview};
use crate::state::dialog_list::{self, DialogSummary};
use crate::state::folders::Folder;
use crate::state::media::{self, PhotoSource};

/// The archive. Folder 0 is the main list; there are no other folders in the API.
const ARCHIVE_FOLDER: i32 = 1;

/// How much of the blocked list to read. Blocked lists are almost always far shorter than this,
/// and the cost of being wrong past it is one menu entry reading "Block" instead of "Unblock".
const BLOCKED_PAGE_SIZE: i32 = 100;

/// Channel endpoints the UI uses to drive the actor.
pub struct Telegram {
    pub commands: mpsc::UnboundedSender<TgCommand>,
    pub events: mpsc::UnboundedReceiver<TgEvent>,
}

/// Connect to Telegram and start the background tasks that keep the connection alive.
pub async fn spawn(config: &Config) -> Result<Telegram> {
    let session = session::open(&config.session_path).await?;

    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::new(Arc::clone(&session), config.api_id);

    let client = Client::new(handle);

    // Drives all MTProto I/O; nothing else works until this is running.
    tokio::spawn(runner.run());

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    tokio::spawn(actor_loop(
        Actor {
            client,
            api_hash: config.api_hash.clone(),
            events: event_tx,
            login_token: None,
            password_token: None,
            dialogs: Arc::new(Mutex::new(None)),
            archive: Arc::new(Mutex::new(ArchiveCursor::default())),
            updates: Some(updates),
        },
        cmd_rx,
    ));

    Ok(Telegram {
        commands: cmd_tx,
        events: event_rx,
    })
}

struct Actor {
    client: Client,
    api_hash: String,
    events: mpsc::UnboundedSender<TgEvent>,
    /// Held between `request_login_code` and `sign_in` so the UI never has to carry it.
    login_token: Option<LoginToken>,
    /// Held between a `PasswordRequired` sign-in failure and `check_password`.
    password_token: Option<PasswordToken>,
    /// One long-lived iterator, so each page picks up where the last left off.
    dialogs: Arc<Mutex<Option<DialogIter>>>,
    /// The same for folder 1, by hand: `DialogIter` hardcodes `folder_id: None` and its request
    /// is private, so the archive is paged through a raw `messages.getDialogs`.
    archive: Arc<Mutex<ArchiveCursor>>,
    /// Taken when the update stream starts; `None` afterwards.
    updates: Option<mpsc::UnboundedReceiver<UpdatesLike>>,
}

/// Where the archive fetch got to.
///
/// `messages.getDialogs` pages by the *last row returned* rather than by an opaque token: the next
/// request repeats that dialog's peer, its top message id, and that message's date. Holding the
/// three together is the whole reason this is a struct.
#[derive(Default)]
struct ArchiveCursor {
    offset_date: i32,
    offset_id: i32,
    offset_peer: Option<tl::enums::InputPeer>,
    exhausted: bool,
}

async fn actor_loop(mut actor: Actor, mut commands: mpsc::UnboundedReceiver<TgCommand>) {
    while let Some(command) = commands.recv().await {
        match command {
            // Login steps are strictly sequential and there is nothing else to do while they
            // run, so they are awaited inline rather than spawned.
            TgCommand::CheckAuthorized => actor.check_authorized().await,
            TgCommand::RequestLoginCode { phone } => actor.request_login_code(&phone).await,
            TgCommand::SignIn { code } => actor.sign_in(&code).await,
            TgCommand::CheckPassword { password } => actor.check_password(&password).await,

            // Everything else is spawned so a slow request (or a flood-wait sleep) never
            // stalls the other commands queued behind it.
            TgCommand::LoadMoreDialogs { archived: false } => actor.load_more_dialogs(),
            TgCommand::LoadMoreDialogs { archived: true } => actor.load_more_archived(),
            TgCommand::LoadFolders => actor.load_folders(),
            TgCommand::OpenChat { peer } => actor.open_chat(peer),
            TgCommand::LoadOlderMessages { peer, before_id } => {
                actor.load_older_messages(peer, before_id)
            }
            TgCommand::DownloadPhoto {
                peer,
                message_id,
                source,
            } => actor.download_photo(peer, message_id, source),
            TgCommand::SendMessage {
                peer,
                text,
                reply_to,
            } => actor.send_message(peer, text, reply_to),
            TgCommand::LoadReplyTargets { peer, ids } => actor.load_reply_targets(peer, ids),

            TgCommand::DeleteMessages { peer, ids, revoke } => {
                actor.delete_messages(peer, ids, revoke)
            }
            TgCommand::EditMessage {
                peer,
                message_id,
                text,
            } => actor.edit_message(peer, message_id, text),
            TgCommand::ForwardMessages {
                source,
                ids,
                destination,
            } => actor.forward_messages(source, ids, destination),

            TgCommand::SetMuted { peer, muted } => actor.set_muted(peer, muted),
            TgCommand::SetPinned { peer, pinned } => actor.set_pinned(peer, pinned),
            TgCommand::SetArchived { peer, archived } => actor.set_archived(peer, archived),
            TgCommand::ClearHistory { peer } => actor.clear_history(peer),
            TgCommand::DeleteDialog { peer } => actor.delete_dialog(peer),
            TgCommand::SetBlocked { peer, blocked } => actor.set_blocked(peer, blocked),
            TgCommand::LoadBlockedPeers => actor.load_blocked_peers(),

            TgCommand::Shutdown => {
                actor.client.disconnect();
                break;
            }
        }
    }
}

impl Actor {
    fn emit(&self, event: TgEvent) {
        // A closed receiver just means the UI is shutting down.
        let _ = self.events.send(event);
    }

    async fn check_authorized(&mut self) {
        match self.client.is_authorized().await {
            Ok(authorized) => self.emit(TgEvent::Authorized(authorized)),
            Err(err) => self.emit(TgEvent::Error(format!("could not reach Telegram: {err}"))),
        }
    }

    async fn request_login_code(&mut self, phone: &str) {
        match self.client.request_login_code(phone, &self.api_hash).await {
            Ok(token) => {
                self.login_token = Some(token);
                self.emit(TgEvent::CodeSent);
            }
            Err(err) => self.emit(TgEvent::LoginFailed(err.to_string())),
        }
    }

    async fn sign_in(&mut self, code: &str) {
        let Some(token) = self.login_token.as_ref() else {
            self.emit(TgEvent::LoginFailed(
                "no login in progress; start over with your phone number".to_string(),
            ));
            return;
        };

        match self.client.sign_in(token, code).await {
            Ok(user) => self.signed_in(user.full_name()),
            Err(SignInError::PasswordRequired(token)) => {
                let hint = token.hint().map(str::to_string);
                self.password_token = Some(token);
                self.emit(TgEvent::PasswordNeeded { hint });
            }
            Err(SignInError::InvalidCode) => {
                self.emit(TgEvent::LoginFailed("that code was not valid".to_string()))
            }
            Err(SignInError::SignUpRequired) => self.emit(TgEvent::LoginFailed(
                "no account for this number; sign up with an official Telegram app first"
                    .to_string(),
            )),
            Err(err) => self.emit(TgEvent::LoginFailed(err.to_string())),
        }
    }

    async fn check_password(&mut self, password: &str) {
        let Some(token) = self.password_token.take() else {
            self.emit(TgEvent::LoginFailed(
                "no password check in progress; start over with your phone number".to_string(),
            ));
            return;
        };

        match self.client.check_password(token, password).await {
            Ok(user) => self.signed_in(user.full_name()),
            Err(SignInError::InvalidPassword(token)) => {
                // Telegram hands back a fresh token so the user can try again.
                self.password_token = Some(token);
                self.emit(TgEvent::LoginFailed("wrong password".to_string()));
            }
            Err(err) => self.emit(TgEvent::LoginFailed(err.to_string())),
        }
    }

    fn signed_in(&mut self, name: String) {
        self.login_token = None;
        self.password_token = None;
        self.emit(TgEvent::SignedIn { name });
    }

    fn load_more_dialogs(&mut self) {
        let client = self.client.clone();
        let dialogs = Arc::clone(&self.dialogs);
        let events = self.events.clone();
        // The very first dialog page is also the cue to start streaming live updates.
        let start_updates = self
            .updates
            .take()
            .map(|updates| (self.client.clone(), self.events.clone(), updates));

        tokio::spawn(async move {
            let mut guard = dialogs.lock().await;
            let iter = guard.get_or_insert_with(|| client.iter_dialogs());

            let mut items = Vec::with_capacity(dialog_list::PAGE_SIZE);
            while items.len() < dialog_list::PAGE_SIZE {
                match iter.next().await {
                    Ok(Some(dialog)) => items.push(DialogSummary::from_grammers(&dialog)),
                    // The iterator stops on its own once the server runs out of dialogs.
                    Ok(None) => break,
                    Err(err) => {
                        let _ = events.send(TgEvent::Error(format!("loading chats: {err}")));
                        return;
                    }
                }
            }

            // The seeded read state is the one thing on screen with no local cause, so log what
            // the server actually said — that is what a wrong tick has to be checked against.
            for item in &items {
                tracing::debug!(
                    name = %item.name,
                    unread = item.unread,
                    read_outbox_max_id = ?item.read_outbox_max_id,
                    "dialog read state"
                );
            }

            let exhausted = items.len() < dialog_list::PAGE_SIZE;
            let _ = events.send(TgEvent::DialogsLoaded {
                items,
                exhausted,
                archived: false,
            });

            // Update gap resolution needs peers in the session cache, which the dialog fetch
            // above has just populated, so this is the first safe moment to start streaming.
            if let Some((client, events, updates)) = start_updates {
                tokio::spawn(stream_updates(client, updates, events));
            }
        });
    }

    /// The archive, one page at a time.
    ///
    /// Hand-rolled because `iter_dialogs` cannot be pointed at a folder: grammers builds the
    /// request with `folder_id: None` and keeps it private. Two things `DialogIter::next` would
    /// have done are therefore skipped — archived peers are not written into the session's peer
    /// cache, and channel `pts` is not recorded for them, so an archived channel resolves an
    /// update gap less precisely. Neither affects reading or sending: the `PeerRef` built from the
    /// response carries its own access hash.
    fn load_more_archived(&mut self) {
        let client = self.client.clone();
        let archive = Arc::clone(&self.archive);
        let events = self.events.clone();

        tokio::spawn(async move {
            let mut cursor = archive.lock().await;
            if cursor.exhausted {
                // The guard in `App` has already been set; report an empty page so it clears.
                let _ = events.send(TgEvent::DialogsLoaded {
                    items: Vec::new(),
                    exhausted: true,
                    archived: true,
                });
                return;
            }

            let request = tl::functions::messages::GetDialogs {
                exclude_pinned: false,
                folder_id: Some(ARCHIVE_FOLDER),
                offset_date: cursor.offset_date,
                offset_id: cursor.offset_id,
                offset_peer: cursor
                    .offset_peer
                    .clone()
                    .unwrap_or(tl::enums::InputPeer::Empty),
                limit: dialog_list::PAGE_SIZE as i32,
                // Only meaningful for the pinned-dialog cache, which this does not keep.
                hash: 0,
            };

            let (dialogs, messages, users, chats, exhausted) = match client.invoke(&request).await {
                // The unsliced form is the whole folder: there is nothing after it.
                Ok(tl::enums::messages::Dialogs::Dialogs(page)) => {
                    (page.dialogs, page.messages, page.users, page.chats, true)
                }
                Ok(tl::enums::messages::Dialogs::Slice(page)) => {
                    (page.dialogs, page.messages, page.users, page.chats, false)
                }
                // Answered only when a hash was sent, which this never does.
                Ok(tl::enums::messages::Dialogs::NotModified(_)) => {
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new(), true)
                }
                Err(err) => {
                    let _ = events.send(TgEvent::Error(format!("loading archive: {err}")));
                    // Report the empty page anyway, or the in-flight guard never clears and
                    // scrolling back down would not retry.
                    let _ = events.send(TgEvent::DialogsLoaded {
                        items: Vec::new(),
                        exhausted: false,
                        archived: true,
                    });
                    return;
                }
            };

            // Each row is kept next to the summary built from it: paging repeats the last one's
            // peer and top message id, and that message's date, and only a row that resolved has
            // a `PeerRef` to repeat.
            let rows: Vec<(&tl::enums::Dialog, DialogSummary)> = dialogs
                .iter()
                .filter_map(|dialog| {
                    DialogSummary::from_raw(dialog, &users, &chats, &messages)
                        .map(|summary| (dialog, summary))
                })
                .collect();

            match rows.last() {
                Some((raw, summary)) => advance(&mut cursor, raw, summary, &messages),
                // Rows came back but not one of them could be resolved to a peer, so there is no
                // anchor for the next request. Stopping is the only way out: repeating the same
                // offsets would fetch the same unusable page forever.
                None if !dialogs.is_empty() => cursor.exhausted = true,
                None => {}
            }
            cursor.exhausted |= exhausted || dialogs.is_empty();

            let items: Vec<DialogSummary> = rows.into_iter().map(|(_, summary)| summary).collect();

            let _ = events.send(TgEvent::DialogsLoaded {
                items,
                exhausted: cursor.exhausted,
                archived: true,
            });
        });
    }

    /// Read the account's chat folders.
    ///
    /// One request for the whole strip: a folder is a rule, not a collection, so there is nothing
    /// to page and nothing to fetch again until an update says the rules changed.
    fn load_folders(&mut self) {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            match fetch_folders(&client).await {
                Ok(folders) => {
                    let _ = events.send(TgEvent::FoldersLoaded { folders });
                }
                // Not fatal: without folders the strip is still "All" and "Archive".
                Err(err) => {
                    let _ = events.send(TgEvent::Error(format!("loading folders: {err}")));
                }
            }
        });
    }

    fn open_chat(&mut self, peer: grammers_session::types::PeerRef) {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            match collect_page(&client, peer, None).await {
                Ok(messages) => {
                    let _ = events.send(TgEvent::MessagesLoaded {
                        peer: peer.id,
                        messages,
                    });
                }
                Err(err) => {
                    let _ = events.send(TgEvent::Error(format!("loading messages: {err}")));
                }
            }
        });
    }

    fn load_older_messages(&mut self, peer: grammers_session::types::PeerRef, before_id: i32) {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            match collect_page(&client, peer, Some(before_id)).await {
                Ok(messages) => {
                    let _ = events.send(TgEvent::OlderMessagesLoaded {
                        peer: peer.id,
                        messages,
                    });
                }
                Err(err) => {
                    // Report an empty page too, so the buffer clears its in-flight guard and
                    // the user can retry by scrolling again.
                    let _ = events.send(TgEvent::OlderMessagesLoaded {
                        peer: peer.id,
                        messages: Vec::new(),
                    });
                    let _ = events.send(TgEvent::Error(format!("loading older messages: {err}")));
                }
            }
        });
    }

    fn download_photo(
        &mut self,
        peer: grammers_session::types::PeerRef,
        message_id: i32,
        source: Box<PhotoSource>,
    ) {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            let image = match fetch_image(&client, &source).await {
                Ok(image) => Some(Arc::new(image)),
                Err(err) => {
                    // Unlike other failures this gets no status banner. A chat can hold dozens
                    // of photos, and the transcript already says what happened by falling back
                    // to the label.
                    tracing::debug!(%err, message_id, "photo download failed");
                    None
                }
            };

            let _ = events.send(TgEvent::PhotoLoaded {
                peer: peer.id,
                message_id,
                image,
            });
        });
    }

    fn send_message(&mut self, peer: PeerRef, text: String, reply_to: Option<i32>) {
        let client = self.client.clone();
        let events = self.events.clone();

        // `reply_to` is a bare message id — quoting part of the parent, or replying across to
        // another topic, would need `InputReplyTo`, which this does not build.
        let message = InputMessage::new().text(text.as_str()).reply_to(reply_to);

        tokio::spawn(async move {
            match client.send_message(peer, message).await {
                Ok(message) => {
                    let _ = events.send(TgEvent::MessageSent {
                        peer: peer.id,
                        message: ChatMessage::from_grammers(&message),
                    });
                }
                Err(err) => {
                    let _ = events.send(TgEvent::Error(format!("could not send message: {err}")));
                }
            }
        });
    }

    /// Fetch the parents of replies on screen, for the line quoted above them.
    ///
    /// `get_messages_by_id` answers index-aligned with the request and puts `None` where a message
    /// is gone. Those gaps are simply left out of `targets`; `asked` is what tells `App` to stop
    /// requesting them. A failed request reports an empty answer for the same reason
    /// `OlderMessagesLoaded` does — the guard has to clear either way, and here it must *stay* set.
    fn load_reply_targets(&mut self, peer: PeerRef, ids: Vec<i32>) {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            let fetched = client.get_messages_by_id(peer, &ids).await;
            if let Err(err) = &fetched {
                tracing::debug!("could not fetch reply targets: {err}");
            }

            let targets = fetched
                .unwrap_or_default()
                .iter()
                .flatten()
                .map(|message| {
                    let flattened = ChatMessage::from_grammers(message);
                    (flattened.id, ReplyPreview::of(&flattened))
                })
                .collect();

            let _ = events.send(TgEvent::ReplyTargetsLoaded {
                peer: peer.id,
                asked: ids,
                targets,
            });
        });
    }

    // -- message actions -----------------------------------------------------

    /// Remove messages, from our copy of the chat or from everybody's.
    ///
    /// `revoke: true` is grammers' own `delete_messages`, which hardcodes exactly that flag.
    /// `revoke: false` has no high-level equivalent and is raw — and it is only ever asked for on
    /// a peer `messages.deleteMessages` accepts, because `channels.deleteMessages` has no such
    /// flag and always deletes for everyone. `actions_for` is what keeps that promise.
    ///
    /// The success event is emitted here rather than left to `updateDeleteMessages`, so the
    /// transcript closes up even if the update is slow. `ChatBuffer::remove` retains, so the echo
    /// arriving as well is harmless.
    fn delete_messages(&mut self, peer: PeerRef, ids: Vec<i32>, revoke: bool) {
        let channel = (peer.id.kind() == PeerKind::Channel).then_some(peer.id);
        let success = TgEvent::MessagesDeleted {
            channel,
            ids: ids.clone(),
        };

        if revoke {
            return self.act(
                "delete this message".to_string(),
                success,
                move |client| async move { client.delete_messages(peer, &ids).await.map(drop) },
            );
        }

        let request = tl::functions::messages::DeleteMessages {
            revoke: false,
            id: ids,
        };
        self.act(
            "delete this message".to_string(),
            success,
            move |client| async move { client.invoke(&request).await.map(drop) },
        );
    }

    /// Replace one of our own messages' text.
    ///
    /// grammers throws away the `Updates` this returns, which is fine here: the same edit arrives
    /// on the update stream as `updateEditMessage`, and `App` already replaces the message in place
    /// when it does — keeping any picture it had decoded. So this event is only for the banner.
    fn edit_message(&mut self, peer: PeerRef, message_id: i32, text: String) {
        self.act(
            "edit this message".to_string(),
            TgEvent::MessageEdited,
            move |client| async move { client.edit_message(peer, message_id, text.as_str()).await },
        );
    }

    /// Copy messages into another conversation.
    ///
    /// The forwarded copies are not applied here: they are ordinary new messages in the
    /// destination and arrive over the update stream, which already appends them and bumps that
    /// chat's row. So the event only says where they went, for the banner.
    fn forward_messages(&mut self, source: PeerRef, ids: Vec<i32>, destination: PeerRef) {
        self.act(
            "forward this message".to_string(),
            TgEvent::MessagesForwarded {
                destination: destination.id,
            },
            move |client| async move {
                client
                    .forward_messages(destination, &ids, source)
                    .await
                    .map(drop)
            },
        );
    }

    // -- chat actions --------------------------------------------------------

    /// Run one chat action, reporting either the confirmed outcome or a banner-worthy failure.
    ///
    /// All of them share this shape, and none applies anything locally on its own — `App` waits
    /// for the event. A failure has to be loud: the menu has closed by the time the answer comes
    /// back, so an unreported one would leave the list looking simply unchanged.
    fn act<F, Fut>(&self, what: String, success: TgEvent, call: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), InvocationError>> + Send + 'static,
    {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            match call(client).await {
                Ok(()) => {
                    let _ = events.send(success);
                }
                Err(err) => {
                    let _ = events.send(TgEvent::Error(format!("could not {what}: {err}")));
                }
            }
        });
    }

    fn set_muted(&mut self, peer: PeerRef, muted: bool) {
        // A mute is a deadline rather than a flag: the second the chat becomes noisy again. A
        // far-future one means "forever", which is the only mute this menu offers, and zero clears
        // it — Telegram has no separate unmute call.
        let request = tl::functions::account::UpdateNotifySettings {
            peer: tl::types::InputNotifyPeer { peer: peer.into() }.into(),
            settings: tl::types::InputPeerNotifySettings {
                show_previews: None,
                silent: None,
                mute_until: Some(if muted { i32::MAX } else { 0 }),
                sound: None,
                stories_muted: None,
                stories_hide_sender: None,
                stories_sound: None,
            }
            .into(),
        };

        self.act(
            format!("{} this chat", if muted { "mute" } else { "unmute" }),
            TgEvent::MuteChanged {
                peer: peer.id,
                muted,
            },
            move |client| async move { client.invoke(&request).await.map(drop) },
        );
    }

    fn set_pinned(&mut self, peer: PeerRef, pinned: bool) {
        let request = tl::functions::messages::ToggleDialogPin {
            pinned,
            peer: tl::types::InputDialogPeer { peer: peer.into() }.into(),
        };

        self.act(
            format!("{} this chat", if pinned { "pin" } else { "unpin" }),
            TgEvent::PinChanged {
                peer: peer.id,
                pinned,
            },
            move |client| async move { client.invoke(&request).await.map(drop) },
        );
    }

    /// Move a chat into the archive, which is folder 1, or back to the main list, which is folder
    /// 0. There is no dedicated archive call — folders are the mechanism, and both directions are
    /// the same request with a different number.
    fn set_archived(&mut self, peer: PeerRef, archived: bool) {
        let request = tl::functions::folders::EditPeerFolders {
            folder_peers: vec![
                tl::types::InputFolderPeer {
                    peer: peer.into(),
                    folder_id: if archived { ARCHIVE_FOLDER } else { 0 },
                }
                .into(),
            ],
        };

        self.act(
            if archived {
                "archive this chat".to_string()
            } else {
                "unarchive this chat".to_string()
            },
            TgEvent::FolderChanged {
                peer: peer.id,
                archived,
            },
            move |client| async move { client.invoke(&request).await.map(drop) },
        );
    }

    fn clear_history(&mut self, peer: PeerRef) {
        // `just_clear` keeps the conversation in the list rather than deleting it, and `revoke`
        // stays false so this empties our copy and leaves the other side's alone. The menu only
        // offers this for peers `messages.deleteHistory` accepts — see `actions_for`.
        let request = tl::functions::messages::DeleteHistory {
            just_clear: true,
            revoke: false,
            peer: peer.into(),
            max_id: 0,
            min_date: None,
            max_date: None,
        };

        self.act(
            "clear this history".to_string(),
            TgEvent::HistoryCleared { peer: peer.id },
            move |client| async move { client.invoke(&request).await.map(drop) },
        );
    }

    /// Delete a private chat, or leave a group or channel: grammers dispatches on peer kind.
    fn delete_dialog(&mut self, peer: PeerRef) {
        let leaving = peer.id.kind() != PeerKind::User;
        self.act(
            if leaving {
                "leave this chat"
            } else {
                "delete this chat"
            }
            .to_string(),
            TgEvent::DialogGone {
                peer: peer.id,
                reason: if leaving { "left" } else { "deleted" },
            },
            move |client| async move { client.delete_dialog(peer).await },
        );
    }

    fn set_blocked(&mut self, peer: PeerRef, blocked: bool) {
        self.act(
            format!("{} this user", if blocked { "block" } else { "unblock" }),
            TgEvent::BlockedChanged {
                peer: peer.id,
                blocked,
            },
            move |client| async move {
                // Two calls rather than one with a flag, so the branch is here rather than in a
                // request builder that would have to be generic over them.
                if blocked {
                    client
                        .invoke(&tl::functions::contacts::Block {
                            my_stories_from: false,
                            id: peer.into(),
                        })
                        .await
                        .map(drop)
                } else {
                    client
                        .invoke(&tl::functions::contacts::Unblock {
                            my_stories_from: false,
                            id: peer.into(),
                        })
                        .await
                        .map(drop)
                }
            },
        );
    }

    /// Read the account's blocked list, so Block/Unblock can show the right face.
    ///
    /// Nothing on a dialog row says whether a user is blocked, and asking per chat would be a
    /// request per row; this answers for the whole account at once. Only the first page is read —
    /// past it a blocked user shows "Block", and blocking twice is harmless.
    fn load_blocked_peers(&mut self) {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            let request = tl::functions::contacts::GetBlocked {
                my_stories_from: false,
                offset: 0,
                limit: BLOCKED_PAGE_SIZE,
            };

            let blocked = match client.invoke(&request).await {
                Ok(tl::enums::contacts::Blocked::Blocked(list)) => list.blocked,
                Ok(tl::enums::contacts::Blocked::Slice(list)) => {
                    if list.count > BLOCKED_PAGE_SIZE {
                        tracing::debug!(
                            total = list.count,
                            read = BLOCKED_PAGE_SIZE,
                            "blocked list truncated; the rest will offer Block rather than Unblock"
                        );
                    }
                    list.blocked
                }
                Err(err) => {
                    // No banner. The list is an optimisation for one menu entry's label, and a
                    // failure here says nothing the user asked to know.
                    tracing::debug!(%err, "could not read the blocked list");
                    return;
                }
            };

            let peers = blocked
                .into_iter()
                .map(|entry| match entry {
                    tl::enums::PeerBlocked::Blocked(entry) => PeerId::from(&entry.peer_id),
                })
                .collect();

            let _ = events.send(TgEvent::BlockedPeersLoaded { peers });
        });
    }
}

/// Fetch one page of history, newest first. `before_id` pages backwards through the chat.
async fn collect_page(
    client: &Client,
    peer: grammers_session::types::PeerRef,
    before_id: Option<i32>,
) -> Result<Vec<ChatMessage>, grammers_client::InvocationError> {
    let mut iter = client.iter_messages(peer).limit(chat_buffer::PAGE_SIZE);
    if let Some(before_id) = before_id {
        // `offset_id` is exclusive, so the page starts just below the oldest message we hold.
        iter = iter.offset_id(before_id);
    }

    let mut messages = Vec::with_capacity(chat_buffer::PAGE_SIZE);
    while let Some(message) = iter.next().await? {
        messages.push(ChatMessage::from_grammers(&message));
    }
    Ok(messages)
}

/// Download a picture into memory and decode it.
///
/// Nothing is written to disk: the data directory is locked down for the session key, and chat
/// photos have no business outliving the process that showed them.
async fn fetch_image(client: &Client, source: &PhotoSource) -> Result<DynamicImage> {
    let mut download = match source {
        // Both are `Downloadable`, but they are distinct types, so the iterator is built here
        // rather than behind a trait object.
        PhotoSource::Thumb(thumb) => client.iter_download(thumb),
        PhotoSource::File(document) => client.iter_download(document),
    };

    let mut bytes = Vec::new();
    while let Some(chunk) = download.next().await? {
        bytes.extend(chunk);
        // A document whose declared mime type lies would otherwise be pulled in whole.
        if bytes.len() > media::MAX_PHOTO_BYTES {
            return Err(eyre!("larger than {} bytes", media::MAX_PHOTO_BYTES));
        }
    }

    // Decoding is CPU-bound and must not sit on a runtime worker while other requests wait.
    tokio::task::spawn_blocking(move || image::load_from_memory(&bytes))
        .await?
        .map_err(Into::into)
}

/// Point the archive cursor just past the last row of a page.
///
/// `messages.getDialogs` has no opaque continuation token: the next request restates where the
/// last one ended, and all three offsets have to agree or the server starts from somewhere else.
/// The date is the *message's*, not the dialog's — a dialog row carries no date of its own.
fn advance(
    cursor: &mut ArchiveCursor,
    raw: &tl::enums::Dialog,
    summary: &DialogSummary,
    messages: &[tl::enums::Message],
) {
    let tl::enums::Dialog::Dialog(dialog) = raw else {
        return;
    };

    cursor.offset_id = dialog.top_message;
    cursor.offset_peer = Some(summary.peer.into());
    cursor.offset_date = messages
        .iter()
        .find_map(|message| match message {
            tl::enums::Message::Message(message)
                if message.id == dialog.top_message && message.peer_id == dialog.peer =>
            {
                Some(message.date)
            }
            tl::enums::Message::Service(message)
                if message.id == dialog.top_message && message.peer_id == dialog.peer =>
            {
                Some(message.date)
            }
            _ => None,
        })
        // A chat whose newest message the server did not send back — an empty one, say. Zero
        // means "no date offset", and the id and peer still pin the position.
        .unwrap_or(0);
}

/// Read the account's chat folders and keep the ones that are folders of the user's own.
async fn fetch_folders(client: &Client) -> Result<Vec<Folder>, InvocationError> {
    let tl::enums::messages::DialogFilters::Filters(answer) = client
        .invoke(&tl::functions::messages::GetDialogFilters {})
        .await?;

    Ok(answer.filters.iter().filter_map(Folder::from_raw).collect())
}

/// Forward live updates for as long as the connection lasts.
async fn stream_updates(
    client: Client,
    updates: mpsc::UnboundedReceiver<UpdatesLike>,
    events: mpsc::UnboundedSender<TgEvent>,
) {
    let mut stream = match client
        .stream_updates(updates, UpdatesConfiguration::default())
        .await
    {
        Ok(stream) => stream,
        Err(err) => {
            let _ = events.send(TgEvent::Error(format!("live updates unavailable: {err}")));
            return;
        }
    };

    loop {
        let update = match stream.next().await {
            Ok(update) => update,
            Err(err) => {
                let _ = events.send(TgEvent::Error(format!("update stream stopped: {err}")));
                return;
            }
        };

        let (message, edited) = match update {
            Update::NewMessage(message) => (message, false),
            Update::MessageEdited(message) => (message, true),
            Update::MessageDeleted(deletion) => {
                // Only channel deletions name their chat; see `App::remove_messages`.
                let channel = deletion.channel_id().map(PeerId::channel_unchecked);
                let _ = events.send(TgEvent::MessagesDeleted {
                    channel,
                    ids: deletion.into_messages(),
                });
                continue;
            }
            // Read state and chat settings never arrive wrapped: grammers builds friendly variants
            // for messages and bot queries only, so these come through raw.
            Update::Raw(raw) => {
                if let Some(event) = read_event(&raw.raw).or_else(|| settings_event(&raw.raw)) {
                    let _ = events.send(event);
                    continue;
                }
                // One update can move several chats at once, so this one answers with a list.
                for event in folder_events(&raw.raw) {
                    let _ = events.send(event);
                }
                // The folders themselves changed shape. They are rules rather than a collection,
                // so there is nothing to patch — the cheapest correct thing is to read them again.
                if matches!(
                    raw.raw,
                    tl::enums::Update::DialogFilter(_)
                        | tl::enums::Update::DialogFilterOrder(_)
                        | tl::enums::Update::DialogFilters
                ) && let Ok(folders) = fetch_folders(&client).await
                {
                    let _ = events.send(TgEvent::FoldersLoaded { folders });
                }
                continue;
            }
            _ => continue,
        };

        // A peer we can't resolve is one we can't route to a buffer either, so skip it.
        let Ok(Some(peer)) = message.peer_ref().await else {
            continue;
        };

        let _ = events.send(TgEvent::IncomingMessage {
            peer,
            message: ChatMessage::from_grammers(&message),
            edited,
        });
    }
}

/// Translate the four read-state updates, resolving the peer here so no `tl` type reaches `App`.
///
/// Unlike a deletion, every one of these names its chat — the two channel forms by bare id, the
/// two history forms by a full `Peer` — so there is no peer ambiguity to guard against.
/// `updateReadChannelDiscussionOutbox` is ignored on purpose: it tracks a comment thread, and
/// tgtui has no thread view for a tick to belong to.
fn read_event(update: &tl::enums::Update) -> Option<TgEvent> {
    use tl::enums::Update as U;
    let event = match update {
        U::ReadHistoryOutbox(read) => TgEvent::OutgoingRead {
            peer: PeerId::from(&read.peer),
            max_id: read.max_id,
        },
        U::ReadChannelOutbox(read) => TgEvent::OutgoingRead {
            peer: PeerId::channel_unchecked(read.channel_id),
            max_id: read.max_id,
        },
        U::ReadHistoryInbox(read) => TgEvent::IncomingRead {
            peer: PeerId::from(&read.peer),
            still_unread: read.still_unread_count,
        },
        U::ReadChannelInbox(read) => TgEvent::IncomingRead {
            peer: PeerId::channel_unchecked(read.channel_id),
            still_unread: read.still_unread_count,
        },
        _ => return None,
    };
    // Read state is the one thing on screen that nothing local ever causes, so when a tick looks
    // wrong the log is the only way to tell "Telegram never said so" from "we decoded it wrong".
    tracing::debug!(?event, "read state");
    Some(event)
}

/// Translate a chat moving between folders, which is what archiving from another device looks
/// like on the wire.
///
/// A list rather than an `Option`: `updateFolderPeers` carries however many chats moved together,
/// and dropping all but the first would leave the rest in the wrong tab until the next start.
/// Folders other than 0 and 1 do not exist — the archive is the only one the API has — so the
/// number is read as a boolean.
fn folder_events(update: &tl::enums::Update) -> Vec<TgEvent> {
    let tl::enums::Update::FolderPeers(update) = update else {
        return Vec::new();
    };

    update
        .folder_peers
        .iter()
        .map(|peer| {
            let tl::enums::FolderPeer::Peer(peer) = peer;
            let event = TgEvent::FolderChanged {
                peer: PeerId::from(&peer.peer),
                archived: peer.folder_id == ARCHIVE_FOLDER,
            };
            tracing::debug!(?event, "folder change");
            event
        })
        .collect()
}

/// Translate the updates that change a conversation's settings rather than its messages.
///
/// These are what makes muting from a phone show up here. The account-wide forms of
/// `updateNotifySettings` — `notifyUsers`, `notifyChats`, `notifyBroadcasts`, `notifyForumTopic` —
/// are dropped: they name no chat, and tgtui has no global notification setting for them to land
/// on. Same reasoning as `updateReadChannelDiscussionOutbox` in `read_event`.
fn settings_event(update: &tl::enums::Update) -> Option<TgEvent> {
    use tl::enums::Update as U;
    let event = match update {
        U::NotifySettings(update) => {
            let tl::enums::NotifyPeer::Peer(notify) = &update.peer else {
                return None;
            };
            let tl::enums::PeerNotifySettings::Settings(settings) = &update.notify_settings;
            TgEvent::MuteChanged {
                peer: PeerId::from(&notify.peer),
                muted: dialog_list::is_muted(settings.mute_until, Utc::now().timestamp()),
            }
        }
        U::PeerBlocked(update) => TgEvent::BlockedChanged {
            peer: PeerId::from(&update.peer_id),
            blocked: update.blocked,
        },
        U::DialogPinned(update) => {
            // `dialogPeerFolder` pins the archive row itself, which is not a conversation.
            let tl::enums::DialogPeer::Peer(dialog) = &update.peer else {
                return None;
            };
            TgEvent::PinChanged {
                peer: PeerId::from(&dialog.peer),
                pinned: update.pinned,
            }
        }
        _ => return None,
    };
    tracing::debug!(?event, "chat settings");
    Some(event)
}
