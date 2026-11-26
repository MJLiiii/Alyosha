//! `alyosha-qq`: relays private (C2C) messages between the owner's QQ
//! account and one `alyosha acp` child, through QQ's official bot API v2.
//!
//! QQ allows four replies per message within 60 minutes and no editing, so
//! the bridge sends no progress messages, keeps replies it cannot deliver,
//! and sends them first when the owner next writes. Group @-mentions from
//! anyone go to the group's guest session. See `docs/develop/QQBridge.md`.

pub mod gateway;
mod images;

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::gateway::GatewayError;

/// The most passive replies QQ allows per message.
pub const REPLY_LIMIT: usize = 4;
pub const CUT_OFF_NOTE: &str = "(cut off — ask me to continue)";
pub const MORE_LATER_NOTE: &str = "(more when you next write)";

/// Codes that mean the passive window for a message is closed.
const PASSIVE_CLOSED: [i64; 3] = [40034128, 40034005, 304103];
/// Codes that mean active messages are refused for now.
const ACTIVE_REFUSED: [i64; 2] = [40034100, 40034105];
const TOO_LONG: i64 = 40054007;

/// A failed send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError {
    pub code: i64,
    pub message: String,
}

/// Where messages go: the real API, or a script in tests.
pub trait Sender: Send + Sync {
    fn send(&self, body: Value) -> futures_util::future::BoxFuture<'_, Result<(), SendError>>;
}

/// Splits a reply into at most four parts of `chunk` characters. Beyond
/// that, the fourth part is the start of the rest with the cut-off note.
pub fn reply_parts(text: &str, chunk: usize) -> Vec<String> {
    cap_parts(alyosha_bridge::split(text, chunk), chunk)
}

fn cap_parts(mut parts: Vec<String>, chunk: usize) -> Vec<String> {
    if parts.len() > REPLY_LIMIT {
        let rest = parts[REPLY_LIMIT - 1..].join("\n");
        parts.truncate(REPLY_LIMIT - 1);
        let room = chunk.saturating_sub(CUT_OFF_NOTE.chars().count() + 1);
        let start: String = rest.chars().take(room).collect();
        parts.push(format!("{start}\n{CUT_OFF_NOTE}"));
    }
    parts
}

/// The reply context of one prompt: the message it answers and its next
/// `msg_seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTo {
    pub msg_id: Option<String>,
    pub seq: u32,
    pub passive_closed: bool,
}

impl ReplyTo {
    pub fn message(msg_id: &str) -> Self {
        Self {
            msg_id: Some(msg_id.to_owned()),
            seq: 1,
            passive_closed: false,
        }
    }

    pub fn scheduled() -> Self {
        Self {
            msg_id: None,
            seq: 1,
            passive_closed: true,
        }
    }
}

/// Sends a reply's parts. Passive replies first; when QQ closes the window,
/// active messages; for a scheduled reply, a recall (`is_wakeup`) message
/// when active ones are refused. Returns the text it could not send, to be
/// sent first after the owner's next message.
pub async fn deliver(sender: &dyn Sender, parts: &[String], reply: &mut ReplyTo) -> Option<String> {
    let scheduled = reply.msg_id.is_none();
    for (index, part) in parts.iter().enumerate() {
        if let Some(msg_id) = reply.msg_id.clone().filter(|_| !reply.passive_closed) {
            match send_passive(sender, part, &msg_id, reply).await {
                Ok(()) => continue,
                Err(error) if PASSIVE_CLOSED.contains(&error.code) => {
                    tracing::warn!(
                        code = error.code,
                        "the reply window closed; sending active messages"
                    );
                    reply.passive_closed = true;
                }
                Err(error) => {
                    tracing::warn!(code = error.code, "a reply failed");
                    return Some(parts[index..].join("\n"));
                }
            }
        }
        match sender.send(json!({"msg_type": 0, "content": part})).await {
            Ok(()) => {}
            Err(error) if scheduled && index == 0 && ACTIVE_REFUSED.contains(&error.code) => {
                tracing::warn!(
                    code = error.code,
                    "active messages refused; trying a recall message"
                );
                let room = 1500usize.saturating_sub(MORE_LATER_NOTE.chars().count() + 1);
                let start: String = part.chars().take(room).collect();
                let wakeup = json!({"msg_type": 0, "content": format!("{start}\n{MORE_LATER_NOTE}"), "is_wakeup": true});
                return match sender.send(wakeup).await {
                    Ok(()) => Some(parts.join("\n")),
                    Err(error) => {
                        tracing::warn!(
                            code = error.code,
                            "the recall message failed; keeping the reply"
                        );
                        Some(parts.join("\n"))
                    }
                };
            }
            Err(error) => {
                tracing::warn!(
                    code = error.code,
                    "an active message failed; keeping the rest"
                );
                return Some(parts[index..].join("\n"));
            }
        }
    }
    None
}

/// Whether QQ has refused native Markdown. Set at the first refusal and
/// kept until the bridge restarts, so later replies skip the attempt.
#[derive(Default)]
pub struct MarkdownRefused(AtomicBool);

/// Sends a model reply. As a passive reply, it first tries QQ's native
/// Markdown (`msg_type: 2`). If QQ refuses that, or the reply is not
/// passive, the reply goes as plain text with the Markdown markers removed,
/// by `deliver`'s rules. Returns what could not be sent, as `deliver` does.
pub async fn deliver_reply(
    sender: &dyn Sender,
    text: &str,
    chunk: usize,
    reply: &mut ReplyTo,
    refused: &MarkdownRefused,
) -> Option<String> {
    if let Some(msg_id) = reply.msg_id.clone().filter(|_| !reply.passive_closed)
        && !refused.0.load(Ordering::Relaxed)
    {
        let parts = cap_parts(alyosha_bridge::markdown::split_markdown(text, chunk), chunk);
        for (index, part) in parts.iter().enumerate() {
            let body = json!({"msg_type": 2, "markdown": {"content": part}, "msg_id": msg_id, "msg_seq": reply.seq});
            reply.seq += 1;
            match sender.send(body).await {
                Ok(()) => continue,
                Err(error) if PASSIVE_CLOSED.contains(&error.code) => reply.passive_closed = true,
                Err(error) if error.code == TOO_LONG => {}
                Err(error) => {
                    tracing::warn!(
                        code = error.code,
                        "QQ refused a Markdown reply; sending plain text until the bridge restarts"
                    );
                    refused.0.store(true, Ordering::Relaxed);
                }
            }
            // The rest goes as plain text.
            let rest = parts[index..].join("\n");
            let plain = alyosha_bridge::markdown::to_plain(&rest);
            return deliver(sender, &reply_parts(&plain, chunk), reply).await;
        }
        return None;
    }
    let plain = alyosha_bridge::markdown::to_plain(text);
    deliver(sender, &reply_parts(&plain, chunk), reply).await
}

async fn send_passive(
    sender: &dyn Sender,
    part: &str,
    msg_id: &str,
    reply: &mut ReplyTo,
) -> Result<(), SendError> {
    let mut body = json!({"msg_type": 0, "content": part, "msg_id": msg_id, "msg_seq": reply.seq});
    reply.seq += 1;
    match sender.send(body.clone()).await {
        Err(error) if error.code == TOO_LONG => {
            tracing::warn!("a part was too long; halving it. Lower reply_chunk.");
            let half = part.chars().count() / 2;
            let first: String = part.chars().take(half).collect();
            let second: String = part.chars().skip(half).collect();
            body["content"] = json!(first);
            body["msg_seq"] = json!(reply.seq);
            reply.seq += 1;
            sender.send(body.clone()).await?;
            body["content"] = json!(second);
            body["msg_seq"] = json!(reply.seq);
            reply.seq += 1;
            sender.send(body).await
        }
        other => other,
    }
}

/// The text of an approval or question, with how to answer it.
pub fn permission_text(title: &str, body: &str, options: &[String], chunk: usize) -> String {
    let instructions = if options.len() == 2 {
        format!("Reply /yes for {} or /no for {}.", options[0], options[1])
    } else {
        let list: Vec<String> = options
            .iter()
            .enumerate()
            .map(|(i, name)| format!("{}. {name}", i + 1))
            .collect();
        format!("{}\nReply with a number.", list.join("\n"))
    };
    let head = if body.is_empty() {
        title.to_owned()
    } else {
        format!("{title}\n\n{body}")
    };
    // The request must fit one reply: cut the body, keeping the instructions.
    let room = chunk.saturating_sub(instructions.chars().count() + 2);
    let head = if head.chars().count() > room {
        let cut: String = head.chars().take(room.saturating_sub(10)).collect();
        format!("{cut}\n(cut off)")
    } else {
        head
    };
    format!("{head}\n\n{instructions}")
}

/// The option an answer picks: `/yes`, `/no`, or a number.
pub fn parse_answer(text: &str, options: usize) -> Option<usize> {
    match text.trim() {
        "/yes" if options == 2 => Some(0),
        "/no" if options == 2 => Some(1),
        number => number
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=options).contains(n))
            .map(|n| n - 1),
    }
}

/// Remembers message IDs for an hour, since QQ may deliver one twice.
#[derive(Default)]
pub struct Seen {
    ids: Mutex<HashMap<String, Instant>>,
}

impl Seen {
    /// True the first time an ID arrives within the hour.
    pub fn first(&self, id: &str, now: Instant) -> bool {
        let mut ids = self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ids.retain(|_, seen| now.saturating_duration_since(*seen) < Duration::from_secs(3600));
        ids.insert(id.to_owned(), now).is_none()
    }
}

/// Reads a token reply: `expires_in` may be a number or a string, and any
/// `code` means failure.
pub fn parse_token(reply: &Value) -> Result<(String, Duration), GatewayError> {
    if let Some(code) = reply.get("code") {
        return Err(GatewayError::Token(format!(
            "code {code}: {}",
            reply["message"].as_str().unwrap_or("no message")
        )));
    }
    let token = reply["access_token"]
        .as_str()
        .ok_or_else(|| GatewayError::Token("no access_token".into()))?;
    let seconds = match &reply["expires_in"] {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
    .ok_or_else(|| GatewayError::Token("no expires_in".into()))?;
    Ok((token.to_owned(), Duration::from_secs(seconds)))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Answers each send with the next scripted result and records the body.
    struct Script {
        results: Mutex<Vec<Result<(), i64>>>,
        sent: Mutex<Vec<Value>>,
    }

    impl Script {
        fn new(results: Vec<Result<(), i64>>) -> Self {
            Self {
                results: Mutex::new(results),
                sent: Mutex::default(),
            }
        }
    }

    impl Sender for Script {
        fn send(&self, body: Value) -> futures_util::future::BoxFuture<'_, Result<(), SendError>> {
            self.sent.lock().unwrap().push(body);
            let mut results = self.results.lock().unwrap();
            let result = if results.is_empty() {
                Ok(())
            } else {
                results.remove(0)
            };
            Box::pin(async move {
                result.map_err(|code| SendError {
                    code,
                    message: String::new(),
                })
            })
        }
    }

    #[test]
    fn a_long_reply_becomes_four_parts_with_a_note() {
        // Five parts' worth: three whole parts, then the start of the rest.
        let text: String = (0..70)
            .map(|n| format!("{n:02} {}\n", "y".repeat(97)))
            .collect();
        let parts = reply_parts(&text, 1500);
        assert_eq!(parts.len(), 4);
        assert!(parts.iter().all(|part| part.chars().count() <= 1500));
        assert!(parts[3].ends_with(CUT_OFF_NOTE));
        // Four parts' worth fits without a note.
        let fits: String = (0..50)
            .map(|n| format!("{n:02} {}\n", "y".repeat(97)))
            .collect();
        let parts = reply_parts(&fits, 1500);
        assert_eq!(parts.len(), 4);
        assert!(!parts[3].ends_with(CUT_OFF_NOTE));
    }

    #[tokio::test]
    async fn passive_replies_count_their_sequence() {
        let script = Script::new(Vec::new());
        let parts = vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ];
        let mut reply = ReplyTo::message("m1");
        assert_eq!(deliver(&script, &parts, &mut reply).await, None);
        let sent = script.sent.lock().unwrap();
        let seqs: Vec<i64> = sent
            .iter()
            .map(|body| body["msg_seq"].as_i64().unwrap())
            .collect();
        assert_eq!(seqs, [1, 2, 3, 4]);
        assert!(sent.iter().all(|body| body["msg_id"] == "m1"));
    }

    #[tokio::test]
    async fn a_closed_window_falls_back_to_active_then_keeps_the_rest() {
        let parts = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        // The third passive send fails; the rest go active.
        let script = Script::new(vec![Ok(()), Ok(()), Err(40034128)]);
        let mut reply = ReplyTo::message("m1");
        assert_eq!(deliver(&script, &parts, &mut reply).await, None);
        {
            let sent = script.sent.lock().unwrap();
            assert_eq!(sent.len(), 4);
            assert!(sent[3].get("msg_id").is_none() && sent[3]["content"] == "c");
        }

        // And when active messages are refused too, the text is kept.
        let script = Script::new(vec![Ok(()), Ok(()), Err(40034128), Err(40034105)]);
        let mut reply = ReplyTo::message("m1");
        assert_eq!(
            deliver(&script, &parts, &mut reply).await,
            Some("c".to_owned())
        );
    }

    #[tokio::test]
    async fn a_scheduled_reply_tries_one_recall_message() {
        let parts = vec!["Stretch now.".to_owned(), "More.".to_owned()];
        let script = Script::new(vec![Err(40034105), Ok(())]);
        let kept = deliver(&script, &parts, &mut ReplyTo::scheduled()).await;
        let sent = script.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1]["is_wakeup"], true);
        assert!(
            sent[1]["content"]
                .as_str()
                .unwrap()
                .ends_with(MORE_LATER_NOTE)
        );
        assert_eq!(
            kept,
            Some("Stretch now.\nMore.".to_owned()),
            "the whole reply waits for the next message"
        );
    }

    #[test]
    fn permissions_say_how_to_answer() {
        let two = permission_text(
            "Approve? google.send",
            "to: anna",
            &["Approve".into(), "Decline".into()],
            1500,
        );
        assert!(two.ends_with("Reply /yes for Approve or /no for Decline."));
        let many = permission_text(
            "Which flight?",
            "",
            &["KL1".into(), "KL2".into(), "None of these".into()],
            1500,
        );
        assert!(many.ends_with("1. KL1\n2. KL2\n3. None of these\nReply with a number."));
        let long = permission_text("T", &"b".repeat(5000), &["Yes".into(), "No".into()], 1500);
        assert!(long.chars().count() <= 1500 && long.contains("(cut off)"));
        assert_eq!(parse_answer("/yes", 2), Some(0));
        assert_eq!(parse_answer("3", 3), Some(2));
        assert_eq!(parse_answer("4", 3), None);
        assert_eq!(parse_answer("/yes", 3), None);
    }

    #[test]
    fn duplicates_are_seen_once_per_hour_and_tokens_parse() {
        let seen = Seen::default();
        let now = Instant::now();
        assert!(seen.first("m1", now));
        assert!(!seen.first("m1", now + Duration::from_secs(10)));
        assert!(seen.first("m1", now + Duration::from_secs(3700)));
        let (token, life) =
            parse_token(&json!({"access_token": "t", "expires_in": "7200"})).unwrap();
        assert_eq!((token.as_str(), life), ("t", Duration::from_secs(7200)));
        assert!(parse_token(&json!({"code": 100016, "message": "invalid appid"})).is_err());
    }
}

#[cfg(test)]
mod markdown_tests {
    use std::sync::Mutex;

    use super::*;

    struct Refuse {
        markdown: Option<i64>,
        sent: Mutex<Vec<Value>>,
    }

    impl Sender for Refuse {
        fn send(&self, body: Value) -> futures_util::future::BoxFuture<'_, Result<(), SendError>> {
            let result = match (body["msg_type"].as_i64(), self.markdown) {
                (Some(2), Some(code)) => Err(SendError {
                    code,
                    message: String::new(),
                }),
                _ => Ok(()),
            };
            self.sent.lock().unwrap().push(body);
            Box::pin(async move { result })
        }
    }

    #[tokio::test]
    async fn replies_go_as_markdown_when_qq_allows_it() {
        let qq = Refuse {
            markdown: None,
            sent: Mutex::default(),
        };
        let refused = MarkdownRefused::default();
        let mut reply = ReplyTo::message("m1");
        assert_eq!(
            deliver_reply(&qq, "**hi** `x`", 1500, &mut reply, &refused).await,
            None
        );
        let sent = qq.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["msg_type"], 2);
        assert_eq!(sent[0]["markdown"]["content"], "**hi** `x`");
        assert_eq!(sent[0]["msg_id"], "m1");
    }

    #[tokio::test]
    async fn a_refusal_sends_clean_text_and_stops_later_attempts() {
        let qq = Refuse {
            markdown: Some(304003),
            sent: Mutex::default(),
        };
        let refused = MarkdownRefused::default();
        let mut reply = ReplyTo::message("m1");
        assert_eq!(
            deliver_reply(&qq, "**hi** `x`", 1500, &mut reply, &refused).await,
            None
        );
        let mut next = ReplyTo::message("m2");
        assert_eq!(
            deliver_reply(&qq, "- a", 1500, &mut next, &refused).await,
            None
        );
        let sent = qq.sent.lock().unwrap();
        let kinds: Vec<(i64, String)> = sent
            .iter()
            .map(|body| {
                (
                    body["msg_type"].as_i64().unwrap(),
                    body["msg_seq"].to_string(),
                )
            })
            .collect();
        // Markdown once (seq 1), its plain resend (seq 2), then plain only.
        assert_eq!(kinds, [(2, "1".into()), (0, "2".into()), (0, "1".into())]);
        assert_eq!(sent[1]["content"], "hi x");
        assert_eq!(sent[2]["content"], "• a");
    }

    #[tokio::test]
    async fn scheduled_replies_are_plain_text() {
        let qq = Refuse {
            markdown: None,
            sent: Mutex::default(),
        };
        let mut reply = ReplyTo::scheduled();
        deliver_reply(
            &qq,
            "**done**",
            1500,
            &mut reply,
            &MarkdownRefused::default(),
        )
        .await;
        let sent = qq.sent.lock().unwrap();
        assert_eq!(
            (sent[0]["msg_type"].as_i64(), sent[0]["content"].as_str()),
            (Some(0), Some("done"))
        );
    }
}
