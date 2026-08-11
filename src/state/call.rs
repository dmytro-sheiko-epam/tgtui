//! Turning a call into a line of text.
//!
//! Telegram reports a call as a *service* message: no text, no media, just an action on the
//! message itself. Left alone it renders as a blank body under a sender header, so it is
//! flattened into a label here, the same way undrawable media becomes `[photo]`.

use chrono::{DateTime, Local};
use grammers_client::tl;

/// Describe a call service message, or `None` when the action is something else entirely.
///
/// `outgoing` is the message's own out flag, which for a call means *we* placed it — the only
/// thing that separates a call the other side never answered from one we gave up on.
pub fn call_label(action: &tl::enums::MessageAction, outgoing: bool) -> Option<String> {
    use tl::enums::PhoneCallDiscardReason as Reason;

    match action {
        tl::enums::MessageAction::PhoneCall(call) => {
            let kind = if call.video { "video call" } else { "call" };
            Some(match &call.reason {
                // Nobody picked up. Which end gave up is the difference between the two.
                Some(Reason::Missed) if outgoing => format!("[cancelled {kind}]"),
                Some(Reason::Missed) => format!("[missed {kind}]"),
                // "Busy" is what the server sends when the callee refused the call.
                Some(Reason::Busy) => format!("[declined {kind}]"),
                // Hung up, dropped, still ringing: the call happened, so say which way it went.
                // A reason of `None` means it hasn't been discarded yet, and carries no duration.
                _ => format!(
                    "[{} {kind}{}]",
                    direction(outgoing),
                    duration_suffix(call.duration)
                ),
            })
        }
        tl::enums::MessageAction::ConferenceCall(call) => Some(if call.missed {
            "[missed group call]".to_string()
        } else {
            format!("[group call{}]", duration_suffix(call.duration))
        }),
        // A group's voice chat, which Telegram's own apps now call a video chat. It is a
        // different thing from a conference call above, so it gets different words: a chat that
        // sits open in the group rather than a call placed to someone. A duration is only sent
        // once it's over.
        tl::enums::MessageAction::GroupCall(call) => Some(match call.duration {
            Some(_) => format!("[video chat ended{}]", duration_suffix(call.duration)),
            None => "[video chat started]".to_string(),
        }),
        // Only user ids come with the invite, and resolving them to names would mean a request
        // from `ChatMessage::from_grammers`, which is synchronous by design.
        tl::enums::MessageAction::InviteToGroupCall(_) => {
            Some("[invited to the video chat]".to_string())
        }
        tl::enums::MessageAction::GroupCallScheduled(call) => {
            Some(match scheduled_at(call.schedule_date) {
                Some(when) => format!("[video chat scheduled for {when}]"),
                None => "[video chat scheduled]".to_string(),
            })
        }
        // `MessageAction` grows with the schema and covers everything from pins to gift codes;
        // anything that isn't a call is left to render as it did before.
        _ => None,
    }
}

fn direction(outgoing: bool) -> &'static str {
    if outgoing { "outgoing" } else { "incoming" }
}

/// When a scheduled video chat starts, in the reader's own zone — a bare "scheduled" would leave
/// out the only part worth reading. `None` for a timestamp the server never should have sent.
fn scheduled_at(timestamp: i32) -> Option<String> {
    Some(
        DateTime::from_timestamp(timestamp as i64, 0)?
            .with_timezone(&Local)
            .format("%b %-d, %H:%M")
            .to_string(),
    )
}

/// ` · 3:21`, or nothing at all — a call that never connected reports zero seconds, and
/// "[missed call · 0:00]" says less than "[missed call]".
fn duration_suffix(seconds: Option<i32>) -> String {
    match seconds {
        Some(seconds) if seconds > 0 => {
            let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
            if hours > 0 {
                format!(" · {hours}:{minutes:02}:{seconds:02}")
            } else {
                format!(" · {minutes}:{seconds:02}")
            }
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone_call(
        video: bool,
        reason: Option<tl::enums::PhoneCallDiscardReason>,
        duration: Option<i32>,
    ) -> tl::enums::MessageAction {
        tl::enums::MessageAction::PhoneCall(tl::types::MessageActionPhoneCall {
            video,
            call_id: 1,
            reason,
            duration,
        })
    }

    fn hung_up(duration: i32) -> tl::enums::MessageAction {
        phone_call(
            false,
            Some(tl::enums::PhoneCallDiscardReason::Hangup),
            Some(duration),
        )
    }

    #[test]
    fn an_answered_call_says_which_way_it_went_and_how_long_it_lasted() {
        assert_eq!(
            call_label(&hung_up(201), false).as_deref(),
            Some("[incoming call · 3:21]")
        );
        assert_eq!(
            call_label(&hung_up(201), true).as_deref(),
            Some("[outgoing call · 3:21]")
        );
    }

    #[test]
    fn an_unanswered_call_was_cancelled_by_whichever_end_gave_up() {
        let missed = phone_call(
            false,
            Some(tl::enums::PhoneCallDiscardReason::Missed),
            Some(0),
        );

        assert_eq!(
            call_label(&missed, false).as_deref(),
            Some("[missed call]"),
            "someone rang us and we never picked up"
        );
        assert_eq!(
            call_label(&missed, true).as_deref(),
            Some("[cancelled call]"),
            "the same reason on our own call means we hung up before they answered"
        );
    }

    #[test]
    fn a_busy_line_is_a_declined_call() {
        let busy = phone_call(false, Some(tl::enums::PhoneCallDiscardReason::Busy), None);

        assert_eq!(call_label(&busy, true).as_deref(), Some("[declined call]"));
    }

    #[test]
    fn a_call_still_ringing_has_no_reason_and_nothing_to_time() {
        assert_eq!(
            call_label(&phone_call(false, None, None), false).as_deref(),
            Some("[incoming call]"),
            "a call that hasn't been discarded yet still deserves a line"
        );
    }

    #[test]
    fn a_call_that_never_connected_shows_no_duration() {
        assert_eq!(
            call_label(&hung_up(0), true).as_deref(),
            Some("[outgoing call]"),
            "'· 0:00' says less than saying nothing"
        );
    }

    #[test]
    fn a_call_past_an_hour_counts_in_hours() {
        assert_eq!(
            call_label(&hung_up(3725), false).as_deref(),
            Some("[incoming call · 1:02:05]")
        );
    }

    #[test]
    fn a_video_call_says_so() {
        let video = phone_call(
            true,
            Some(tl::enums::PhoneCallDiscardReason::Missed),
            Some(0),
        );

        assert_eq!(
            call_label(&video, false).as_deref(),
            Some("[missed video call]")
        );
    }

    #[test]
    fn a_conference_call_is_a_group_call() {
        let call = |missed, duration| {
            tl::enums::MessageAction::ConferenceCall(tl::types::MessageActionConferenceCall {
                missed,
                active: false,
                video: false,
                call_id: 1,
                duration,
                other_participants: None,
            })
        };

        assert_eq!(
            call_label(&call(false, Some(300)), false).as_deref(),
            Some("[group call · 5:00]")
        );
        assert_eq!(
            call_label(&call(true, None), false).as_deref(),
            Some("[missed group call]")
        );
    }

    #[test]
    fn a_groups_video_chat_says_whether_it_is_still_going() {
        let voice_chat = |duration| {
            tl::enums::MessageAction::GroupCall(tl::types::MessageActionGroupCall {
                call: tl::enums::InputGroupCall::Call(tl::types::InputGroupCall {
                    id: 1,
                    access_hash: 1,
                }),
                duration,
            })
        };

        assert_eq!(
            call_label(&voice_chat(None), false).as_deref(),
            Some("[video chat started]"),
            "a duration only arrives once the chat is over, so its absence means it is open"
        );
        assert_eq!(
            call_label(&voice_chat(Some(723)), false).as_deref(),
            Some("[video chat ended · 12:03]")
        );
    }

    #[test]
    fn an_invitation_to_a_video_chat_is_labelled_without_naming_anyone() {
        let invite = tl::enums::MessageAction::InviteToGroupCall(
            tl::types::MessageActionInviteToGroupCall {
                call: tl::enums::InputGroupCall::Call(tl::types::InputGroupCall {
                    id: 1,
                    access_hash: 1,
                }),
                users: vec![7, 8],
            },
        );

        assert_eq!(
            call_label(&invite, false).as_deref(),
            Some("[invited to the video chat]"),
            "only ids come with the invite, and resolving them would need a request"
        );
    }

    #[test]
    fn a_scheduled_video_chat_says_when() {
        let scheduled = tl::enums::MessageAction::GroupCallScheduled(
            tl::types::MessageActionGroupCallScheduled {
                call: tl::enums::InputGroupCall::Call(tl::types::InputGroupCall {
                    id: 1,
                    access_hash: 1,
                }),
                schedule_date: 1_700_000_000,
            },
        );

        let expected = format!(
            "[video chat scheduled for {}]",
            DateTime::from_timestamp(1_700_000_000, 0)
                .unwrap()
                .with_timezone(&Local)
                .format("%b %-d, %H:%M")
        );
        assert_eq!(
            call_label(&scheduled, false).as_deref(),
            Some(expected.as_str()),
            "the time is the only part of a scheduled chat worth reading, and it is the reader's own"
        );
    }

    #[test]
    fn a_service_message_that_is_not_a_call_has_no_label() {
        assert!(
            call_label(&tl::enums::MessageAction::ChatDeletePhoto, false).is_none(),
            "everything else must keep rendering exactly as it did"
        );
    }
}
