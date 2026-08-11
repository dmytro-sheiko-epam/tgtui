//! The Telegram actor: the only part of the app that talks to grammers.
//!
//! The UI never awaits a network call. It pushes a [`TgCommand`] down a channel and later
//! receives a [`TgEvent`] back, which keeps the render loop responsive while requests are in
//! flight.

pub mod commands;
pub mod events;
pub mod session;

use std::sync::Arc;

use color_eyre::eyre::{Result, eyre};
use grammers_client::client::{DialogIter, LoginToken, PasswordToken, UpdatesConfiguration};
use grammers_client::tl;
use grammers_client::update::Update;
use grammers_client::{Client, SenderPool, SignInError};
use grammers_session::types::PeerId;
use grammers_session::updates::UpdatesLike;
use image::DynamicImage;
use tokio::sync::{Mutex, mpsc};

pub use commands::TgCommand;
pub use events::TgEvent;

use crate::config::Config;
use crate::state::chat_buffer::{self, ChatMessage};
use crate::state::dialog_list::{self, DialogSummary};
use crate::state::media::{self, PhotoSource};

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
    /// Taken when the update stream starts; `None` afterwards.
    updates: Option<mpsc::UnboundedReceiver<UpdatesLike>>,
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
            TgCommand::LoadMoreDialogs => actor.load_more_dialogs(),
            TgCommand::OpenChat { peer } => actor.open_chat(peer),
            TgCommand::LoadOlderMessages { peer, before_id } => {
                actor.load_older_messages(peer, before_id)
            }
            TgCommand::DownloadPhoto {
                peer,
                message_id,
                source,
            } => actor.download_photo(peer, message_id, source),
            TgCommand::SendMessage { peer, text } => actor.send_message(peer, text),

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
            let _ = events.send(TgEvent::DialogsLoaded { items, exhausted });

            // Update gap resolution needs peers in the session cache, which the dialog fetch
            // above has just populated, so this is the first safe moment to start streaming.
            if let Some((client, events, updates)) = start_updates {
                tokio::spawn(stream_updates(client, updates, events));
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

    fn send_message(&mut self, peer: grammers_session::types::PeerRef, text: String) {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            match client.send_message(peer, text.as_str()).await {
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
            // Read state never arrives wrapped: grammers builds friendly variants for messages and
            // bot queries only, so these come through raw.
            Update::Raw(raw) => {
                if let Some(event) = read_event(&raw.raw) {
                    let _ = events.send(event);
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
