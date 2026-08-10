//! The full-screen picture viewer.
//!
//! Modal and full-bleed: it replaces both panes rather than floating over them, because the whole
//! point is to give a picture every row and column the terminal has.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use ratatui_image::sliced::{SignedPosition, SlicedImage};

use crate::app::App;
use crate::state::media::{PhotoRef, PhotoState};
use crate::ui::images::ImageStore;
use crate::ui::text::wrap;
use crate::ui::widgets::pane;

/// Rows kept below the picture for the caption. The picture is fitted to what is left, so a long
/// caption never pushes it off the bottom.
const CAPTION_ROWS: u16 = 2;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, images: &mut ImageStore) {
    let Some(message_id) = app.viewer else {
        return;
    };

    let title = title(app, message_id);
    let block = pane(&title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some((photo, caption)) = photo(app, message_id) else {
        // The message went away between the keypress and the frame.
        frame.render_widget(note("this message is gone"), inner);
        return;
    };

    let caption = caption.to_string();
    let has_caption = !caption.is_empty();
    let picture_rows = if has_caption {
        inner.height.saturating_sub(CAPTION_ROWS)
    } else {
        inner.height
    };

    let drawn = photo
        .image()
        .and_then(|image| images.prepare(message_id, image, inner.width, picture_rows))
        .map(|size| (size, image_area(inner, size, picture_rows)));

    match drawn {
        Some((size, picture)) => {
            if let Some(protocol) = images.protocol(message_id, size) {
                let offset = SignedPosition::from((
                    (picture.x - inner.x) as i16,
                    (picture.y - inner.y) as i16,
                ));
                frame.render_widget(SlicedImage::new(protocol, offset), inner);
            }
            if has_caption {
                let below = Rect {
                    y: inner.y + picture_rows,
                    height: inner.height - picture_rows,
                    ..inner
                };
                frame.render_widget(
                    Paragraph::new(wrap(&caption, inner.width as usize).join("\n"))
                        .alignment(Alignment::Center),
                    below,
                );
            }
        }
        None => frame.render_widget(note(waiting_for(photo)), inner),
    }

    // The same guard path the transcript uses, so a picture that was never scrolled into view
    // downloads when you step onto it.
    app.request_visible_photos(&[message_id]);
}

/// `photo · Alice · 2/5` — enough to know whose picture it is and how far through you are.
fn title(app: &App, message_id: i32) -> String {
    let who = app
        .open_buffer()
        .and_then(|buffer| buffer.messages.iter().find(|m| m.id == message_id))
        .map(|message| {
            if message.outgoing {
                "you".to_string()
            } else {
                message.sender.clone().unwrap_or_else(|| "—".to_string())
            }
        })
        .unwrap_or_else(|| "—".to_string());

    match app.viewer_position() {
        Some((nth, total)) if total > 1 => format!("photo · {who} · {nth}/{total}"),
        _ => format!("photo · {who}"),
    }
}

fn photo(app: &App, message_id: i32) -> Option<(&PhotoRef, &str)> {
    let message = app
        .open_buffer()?
        .messages
        .iter()
        .find(|m| m.id == message_id)?;
    Some((message.photo.as_ref()?, message.text.as_str()))
}

/// Centre the picture in the space above the caption.
fn image_area(inner: Rect, size: Size, picture_rows: u16) -> Rect {
    let available = Rect {
        height: picture_rows,
        ..inner
    };
    crate::ui::widgets::centered(available, size.width, size.height)
}

fn waiting_for(photo: &PhotoRef) -> &'static str {
    match photo.state {
        PhotoState::Failed => "this picture could not be downloaded",
        // Includes the case where the terminal has no graphics protocol at all, which is why
        // this is worded about the picture rather than about the download.
        _ => "loading picture...",
    }
}

fn note(text: &str) -> Paragraph<'_> {
    Paragraph::new(text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::chat_buffer::ChatBuffer;
    use crate::test_support::{loaded_photo_message, peer};

    fn app_with(messages: Vec<crate::state::chat_buffer::ChatMessage>) -> App {
        let (mut app, _rx) = crate::test_support::app();
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(Vec::new());
        buffer.messages.extend(messages);
        app.open_chat = Some(peer(1).id);
        app.chats.insert(peer(1).id, buffer);
        app
    }

    #[test]
    fn the_title_says_whose_picture_it_is_and_how_far_through_you_are() {
        let mut app = app_with(vec![
            loaded_photo_message(1, "", 100, 200),
            loaded_photo_message(2, "", 100, 200),
        ]);
        app.viewer = Some(2);

        assert_eq!(title(&app, 2), "photo · Alice · 2/2");
    }

    #[test]
    fn a_lone_picture_is_not_numbered() {
        let mut app = app_with(vec![loaded_photo_message(1, "", 100, 200)]);
        app.viewer = Some(1);

        assert_eq!(
            title(&app, 1),
            "photo · Alice",
            "'1/1' is noise when there is nowhere to step to"
        );
    }

    #[test]
    fn a_picture_is_centred_in_the_space_above_its_caption() {
        let inner = Rect::new(0, 0, 40, 20);

        let area = image_area(inner, Size::new(10, 6), 18);

        assert_eq!((area.x, area.width), (15, 10), "centred horizontally");
        assert_eq!(
            (area.y, area.height),
            (6, 6),
            "centred in the 18 rows above the caption, not the full 20"
        );
    }
}
