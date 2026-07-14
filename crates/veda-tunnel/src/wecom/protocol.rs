//! Wire types for the WeCom (企业微信) aibot long-connection protocol.
//!
//! Envelope shape verified against the official doc (2026-07-09,
//! developer.work.weixin.qq.com/document/path/101463): pushes are
//! `{cmd, headers:{req_id}, body:{...}}`, while `aibot_subscribe` / `ping`
//! ACKs come back as `{headers, errcode, errmsg}` WITHOUT a `cmd`.
//!
//! Inbound frames are parsed leniently via [`RawFrame`] (every field
//! optional) and dispatched on `cmd`, so an unrecognised or restructured
//! frame degrades to "ignored" instead of tearing down the connection.
//! Per-field structure of new frame types must still be re-checked against
//! the official doc before trusting them.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const CMD_SUBSCRIBE: &str = "aibot_subscribe";
pub const CMD_PING: &str = "ping";
pub const CMD_RESPOND_MSG: &str = "aibot_respond_msg";
pub const CMD_MSG_CALLBACK: &str = "aibot_msg_callback";
pub const CMD_EVENT_CALLBACK: &str = "aibot_event_callback";

pub const EVENT_DISCONNECTED: &str = "disconnected_event";
pub const EVENT_ENTER_CHAT: &str = "enter_chat";
pub const EVENT_FEEDBACK: &str = "feedback_event";

pub const MSGTYPE_TEXT: &str = "text";

// ── Uplink (tunnel → WeCom) ─────────────────────────────
// Built as `Value` (not typed structs) to keep the wire shape obvious and
// avoid lifetime noise; the frames are tiny and fixed.

pub fn subscribe_frame(req_id: &str, bot_id: &str, secret: &str) -> Value {
    json!({
        "cmd": CMD_SUBSCRIBE,
        "headers": { "req_id": req_id },
        "body": { "bot_id": bot_id, "secret": secret }
    })
}

pub fn ping_frame(req_id: &str) -> Value {
    json!({
        "cmd": CMD_PING,
        "headers": { "req_id": req_id }
    })
}

/// One frame of a streaming reply. Reuse the same `stream_id` across frames
/// to refresh the same bubble; set `finish=true` on the last one. `req_id`
/// should echo the triggering callback's req_id.
///
/// `feedback_id`: set on the FIRST frame of a reply to activate WeCom's
/// thumb-up/down UI on the bubble (`stream.feedback.id`, ≤256 bytes; doc
/// 101031). Votes come back as `feedback_event` callbacks carrying this id.
pub fn respond_stream_frame(
    req_id: &str,
    stream_id: &str,
    content: &str,
    finish: bool,
    feedback_id: Option<&str>,
) -> Value {
    let mut stream = json!({ "id": stream_id, "finish": finish, "content": content });
    if let Some(fid) = feedback_id {
        stream["feedback"] = json!({ "id": fid });
    }
    json!({
        "cmd": CMD_RESPOND_MSG,
        "headers": { "req_id": req_id },
        "body": { "msgtype": "stream", "stream": stream }
    })
}

// ── Downlink (WeCom → tunnel) ───────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Headers {
    #[serde(default)]
    pub req_id: String,
}

/// Lenient view of any inbound frame. `cmd` present → a push; `cmd` absent
/// with `errcode` present → a subscribe/ping ACK.
#[derive(Debug, Deserialize)]
pub struct RawFrame {
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub headers: Headers,
    #[serde(default)]
    pub errcode: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub body: Option<Value>,
}

impl RawFrame {
    /// True for a subscribe/ping ACK reporting success (`errcode == 0`).
    pub fn is_ok_ack(&self) -> bool {
        self.cmd.is_none() && self.errcode == Some(0)
    }
}

#[derive(Debug, Deserialize)]
pub struct MsgCallbackBody {
    pub msgid: String,
    #[serde(default)]
    pub chattype: Option<String>,
    /// Group conversation id — present on group messages only (doc 100719);
    /// single chats are keyed by `from.userid` instead.
    #[serde(default)]
    pub chatid: Option<String>,
    #[serde(default)]
    pub from: Option<MsgFrom>,
    #[serde(default)]
    pub msgtype: Option<String>,
    #[serde(default)]
    pub text: Option<TextContent>,
}

#[derive(Debug, Deserialize)]
pub struct MsgFrom {
    #[serde(default)]
    pub userid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TextContent {
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct EventCallbackBody {
    #[serde(default)]
    pub msgid: Option<String>,
    #[serde(default)]
    pub from: Option<MsgFrom>,
    #[serde(default)]
    pub event: Option<EventInner>,
}

#[derive(Debug, Deserialize)]
pub struct EventInner {
    #[serde(default)]
    pub eventtype: String,
    /// Present when `eventtype == feedback_event`. Field names live-verified
    /// against a real frame (2026-07-14): `{"id": "<our feedback.id uuid>",
    /// "type": 1|2, "inaccurate_reason_list": [..]}` — type 1 = thumb-up,
    /// 2 = thumb-down (reasons only accompany down-votes).
    #[serde(default)]
    pub feedback_event: Option<FeedbackEvent>,
}

#[derive(Debug, Deserialize)]
pub struct FeedbackEvent {
    #[serde(default)]
    pub id: String,
    #[serde(default, rename = "type")]
    pub kind: i8,
    #[serde(default)]
    pub inaccurate_reason_list: Vec<i8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_frame_shape() {
        let f = subscribe_frame("r1", "bot1", "sec");
        assert_eq!(f["cmd"], CMD_SUBSCRIBE);
        assert_eq!(f["headers"]["req_id"], "r1");
        assert_eq!(f["body"]["bot_id"], "bot1");
        assert_eq!(f["body"]["secret"], "sec");
    }

    #[test]
    fn respond_stream_frame_shape() {
        let f = respond_stream_frame("r1", "s1", "hello", false, None);
        assert_eq!(f["body"]["msgtype"], "stream");
        assert_eq!(f["body"]["stream"]["id"], "s1");
        assert_eq!(f["body"]["stream"]["finish"], false);
        assert_eq!(f["body"]["stream"]["content"], "hello");
    }

    #[test]
    fn parses_msg_callback() {
        let raw: RawFrame = serde_json::from_str(
            r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"r1"},
                "body":{"msgid":"m1","chattype":"group","from":{"userid":"u1"},
                        "msgtype":"text","text":{"content":"@Robot 报销流程"}}}"#,
        )
        .unwrap();
        assert_eq!(raw.cmd.as_deref(), Some(CMD_MSG_CALLBACK));
        let body: MsgCallbackBody = serde_json::from_value(raw.body.unwrap()).unwrap();
        assert_eq!(body.msgid, "m1");
        assert_eq!(body.msgtype.as_deref(), Some(MSGTYPE_TEXT));
        assert_eq!(body.text.unwrap().content, "@Robot 报销流程");
    }

    #[test]
    fn parses_disconnected_event() {
        let raw: RawFrame = serde_json::from_str(
            r#"{"cmd":"aibot_event_callback","headers":{"req_id":"r1"},
                "body":{"msgid":"m1","msgtype":"event",
                        "event":{"eventtype":"disconnected_event"}}}"#,
        )
        .unwrap();
        assert_eq!(raw.cmd.as_deref(), Some(CMD_EVENT_CALLBACK));
        let body: EventCallbackBody = serde_json::from_value(raw.body.unwrap()).unwrap();
        assert_eq!(body.event.unwrap().eventtype, EVENT_DISCONNECTED);
    }

    #[test]
    fn parses_feedback_event_live_frame() {
        // Verbatim frame captured from a live bot (2026-07-14): thumb-down
        // with one inaccuracy reason, after the same user's earlier thumb-up.
        let raw: RawFrame = serde_json::from_str(
            r#"{"cmd":"aibot_event_callback","headers":{"req_id":"r1"},
                "body":{"aibotid":"aib9x","chattype":"single","create_time":1784000537,
                        "event":{"eventtype":"feedback_event",
                                 "feedback_event":{"id":"f62ed7b7-f697-4621-8223-d133768f1981",
                                                   "inaccurate_reason_list":[2],"type":2}},
                        "from":{"userid":"wo-ApVEAAAnC"},"msgid":"m2","msgtype":"event"}}"#,
        )
        .unwrap();
        let body: EventCallbackBody = serde_json::from_value(raw.body.unwrap()).unwrap();
        assert_eq!(body.from.as_ref().unwrap().userid.as_deref(), Some("wo-ApVEAAAnC"));
        let ev = body.event.unwrap();
        assert_eq!(ev.eventtype, EVENT_FEEDBACK);
        let fe = ev.feedback_event.unwrap();
        assert_eq!(fe.id, "f62ed7b7-f697-4621-8223-d133768f1981");
        assert_eq!(fe.kind, 2);
        assert_eq!(fe.inaccurate_reason_list, vec![2]);
    }

    #[test]
    fn parses_feedback_up_without_reasons() {
        let raw: RawFrame = serde_json::from_str(
            r#"{"cmd":"aibot_event_callback","headers":{"req_id":"r1"},
                "body":{"event":{"eventtype":"feedback_event",
                                 "feedback_event":{"id":"fid-1","type":1}},
                        "from":{"userid":"u1"},"msgid":"m1","msgtype":"event"}}"#,
        )
        .unwrap();
        let body: EventCallbackBody = serde_json::from_value(raw.body.unwrap()).unwrap();
        let fe = body.event.unwrap().feedback_event.unwrap();
        assert_eq!(fe.kind, 1);
        assert!(fe.inaccurate_reason_list.is_empty());
    }

    #[test]
    fn stream_frame_carries_feedback_id_on_first_frame_only() {
        let first = respond_stream_frame("r1", "s1", "…", false, Some("fid-9"));
        assert_eq!(first["body"]["stream"]["feedback"]["id"], "fid-9");
        let last = respond_stream_frame("r1", "s1", "done", true, None);
        assert!(last["body"]["stream"].get("feedback").is_none());
    }

    #[test]
    fn recognises_ok_ack() {
        let raw: RawFrame =
            serde_json::from_str(r#"{"headers":{"req_id":"r1"},"errcode":0,"errmsg":"ok"}"#)
                .unwrap();
        assert!(raw.is_ok_ack());
    }

    #[test]
    fn error_ack_is_not_ok() {
        let raw: RawFrame =
            serde_json::from_str(r#"{"headers":{"req_id":"r1"},"errcode":40001,"errmsg":"bad"}"#)
                .unwrap();
        assert!(!raw.is_ok_ack());
    }
}
