#![deny(unsafe_code)]

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client as HttpClient, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;

const ERR_TOKEN_NOT_MEMBER: &str = "not a member of this room";
const ERR_TOKEN_PERMISSION_DENIED: &str = "permission denied";

#[derive(Debug, Clone)]
pub struct Client {
    base_url: String,
    token: String,
    http_client: HttpClient,
}

impl Client {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            http_client: HttpClient::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build reqwest client"),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn create_message(&self, room_id: &str, body: &str) -> Result<(), Error> {
        self.do_rpc::<_, serde_json::Value>(
            "chatto.api.v1.MessageService",
            "CreateMessage",
            Some(json!({"roomId": room_id, "body": body})),
        )
        .await
        .map(|_| ())
    }

    /// The cursor is a oneof (`before`/`after`) serialized at the top level of
    /// the request in ProtoJSON.
    pub async fn get_room_events(
        &self,
        room_id: &str,
        after_cursor: &str,
        limit: i32,
    ) -> Result<GetRoomEventsResponse, Error> {
        let req = if after_cursor.is_empty() {
            json!({"roomId": room_id, "limit": limit})
        } else {
            json!({"roomId": room_id, "limit": limit, "after": after_cursor})
        };

        self.do_rpc("chatto.api.v1.RoomService", "GetRoomEvents", Some(req))
            .await
    }

    pub async fn get_viewer(&self) -> Result<String, Error> {
        let profile = self.get_profile().await?;
        Ok(profile.id)
    }

    pub async fn get_profile(&self) -> Result<UserProfile, Error> {
        #[derive(Debug, Deserialize)]
        struct Resp {
            user: User,
        }
        #[derive(Debug, Deserialize)]
        struct User {
            profile: UserProfile,
        }
        let resp: Resp = self
            .do_rpc("chatto.api.v1.ViewerService", "GetViewer", Some(json!({})))
            .await?;
        Ok(resp.user.profile)
    }

    pub async fn add_member(&self, room_id: &str, user_id: &str) -> Result<(), Error> {
        self.do_rpc::<_, serde_json::Value>(
            "chatto.api.v1.RoomService",
            "AddMember",
            Some(json!({"roomId": room_id, "userId": user_id})),
        )
        .await
        .map(|_| ())
    }

    pub async fn join_room(&self, room_id: &str) -> Result<(), Error> {
        self.do_rpc::<_, serde_json::Value>(
            "chatto.api.v1.RoomService",
            "JoinRoom",
            Some(json!({"roomId": room_id})),
        )
        .await
        .map(|_| ())
    }

    pub async fn join_call(&self, room_id: &str) -> Result<bool, Error> {
        #[derive(Debug, Deserialize)]
        struct Resp {
            joined: bool,
        }
        let resp: Resp = self
            .do_rpc(
                "chatto.api.v1.VoiceCallService",
                "JoinCall",
                Some(json!({"roomId": room_id})),
            )
            .await?;
        Ok(resp.joined)
    }

    /// Chatto 0.5 renamed `GetCallToken` to `CreateCallToken` (same request
    /// and response fields, old route removed).
    pub async fn create_call_token(&self, room_id: &str) -> Result<CallToken, Error> {
        self.do_rpc(
            "chatto.api.v1.VoiceCallService",
            "CreateCallToken",
            Some(json!({"roomId": room_id})),
        )
        .await
    }

    pub async fn leave_call(&self, room_id: &str) -> Result<bool, Error> {
        #[derive(Debug, Deserialize)]
        struct Resp {
            left: bool,
        }
        let resp: Resp = self
            .do_rpc(
                "chatto.api.v1.VoiceCallService",
                "LeaveCall",
                Some(json!({"roomId": room_id})),
            )
            .await?;
        Ok(resp.left)
    }

    /// Chatto 0.5 renamed `UpdatePresence` to `SetPresence`.
    pub async fn set_presence(&self, status: &str, user_selected: bool) -> Result<(), Error> {
        self.do_rpc::<_, serde_json::Value>(
            "chatto.api.v1.MyAccountService",
            "SetPresence",
            Some(json!({"status": status, "userSelected": user_selected})),
        )
        .await
        .map(|_| ())
    }

    pub async fn upload_avatar(&self, user_id: &str, image_data: &[u8]) -> Result<(), Error> {
        let url = format!(
            "{}/api/connect/chatto.api.v1.UserService/UploadAvatar",
            self.base_url
        );

        // ImageUpload { bytes image = 1; }
        let mut inner = Vec::with_capacity(image_data.len() + 10);
        inner.push(0x0A);
        encode_varint(&mut inner, image_data.len() as u64);
        inner.extend_from_slice(image_data);

        // UploadAvatarRequest { ImageUpload image = 4; string user_id = 5; }
        let mut outer = Vec::with_capacity(inner.len() + user_id.len() + 20);
        outer.push(0x22);
        encode_varint(&mut outer, inner.len() as u64);
        outer.extend_from_slice(&inner);
        outer.push(0x2A);
        encode_varint(&mut outer, user_id.len() as u64);
        outer.extend_from_slice(user_id.as_bytes());

        let mut request = self
            .http_client
            .request(Method::POST, &url)
            .header(CONTENT_TYPE, "application/proto")
            .header("connect-protocol-version", "1")
            .body(outer);

        if !self.token.is_empty() {
            request = request.header(AUTHORIZATION, format!("Bearer {}", self.token));
        }

        let response = request.send().await.map_err(Error::Http)?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.map_err(Error::Http)?;
            return Err(Error::Rpc(RpcError {
                status_code: status,
                url,
                body: truncate(&body, 500),
            }));
        }

        Ok(())
    }

    async fn do_rpc<Req, Resp>(
        &self,
        service: &str,
        method: &str,
        req: Option<Req>,
    ) -> Result<Resp, Error>
    where
        Req: Serialize,
        Resp: for<'de> Deserialize<'de>,
    {
        let url = format!("{}/api/connect/{service}/{method}", self.base_url);

        let mut request = self
            .http_client
            .request(Method::POST, &url)
            .header(CONTENT_TYPE, "application/json");

        if !self.token.is_empty() {
            request = request.header(AUTHORIZATION, format!("Bearer {}", self.token));
        }

        if let Some(req_body) = req {
            request = request.json(&req_body);
        }

        let response = request.send().await.map_err(Error::Http)?;
        let status = response.status();
        let body = response.text().await.map_err(Error::Http)?;

        if !status.is_success() {
            return Err(Error::Rpc(RpcError {
                status_code: status,
                url,
                body: truncate(&body, 500),
            }));
        }

        let parse_body = if body.trim().is_empty() {
            "null"
        } else {
            &body
        };

        serde_json::from_str(parse_body).map_err(|source| Error::Unmarshal {
            status_code: status,
            url,
            source,
            body: truncate(&body, 1000),
        })
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("http request: {0}")]
    Http(reqwest::Error),
    #[error(transparent)]
    Rpc(RpcError),
    #[error("unmarshal response (status {status_code}) for {url}: {source}\nbody: {body}")]
    Unmarshal {
        status_code: StatusCode,
        url: String,
        source: serde_json::Error,
        body: String,
    },
}

#[derive(Debug, Error)]
#[error("RPC error (status {status_code}) for {url}: {body}")]
pub struct RpcError {
    pub status_code: StatusCode,
    pub url: String,
    pub body: String,
}

pub fn is_not_member_error(err: &Error) -> bool {
    match err {
        Error::Rpc(rpc) => {
            let body = rpc.body.to_ascii_lowercase();
            body.contains(ERR_TOKEN_NOT_MEMBER) || body.contains("not_found")
        }
        _ => false,
    }
}

pub fn is_permission_denied_error(err: &Error) -> bool {
    match err {
        Error::Rpc(rpc) => {
            let body = rpc.body.to_ascii_lowercase();
            body.contains(ERR_TOKEN_PERMISSION_DENIED) || body.contains("permission_denied")
        }
        _ => false,
    }
}

pub fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        if value < 0x80 {
            buf.push(value as u8);
            break;
        }
        buf.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
}

pub fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_owned();
    }
    let mut idx = max_len;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    s[..idx].to_owned()
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct GetRoomEventsResponse {
    pub page: Option<RoomTimelinePage>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    pub id: String,
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RoomTimelinePage {
    #[serde(default)]
    pub events: Vec<RoomTimelineEvent>,
    #[serde(default)]
    pub start_cursor: String,
    #[serde(default)]
    pub end_cursor: String,
    #[serde(default)]
    pub has_older: bool,
    #[serde(default)]
    pub has_newer: bool,
    #[serde(default)]
    pub includes: Option<RoomTimelineIncludes>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RoomTimelineIncludes {
    #[serde(default)]
    pub users: HashMap<String, UserProfile>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RoomTimelineEvent {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub actor_id: String,
    pub message_posted: Option<RoomMessagePosted>,
    pub room_created: Option<RoomEventMeta>,
    pub user_joined_room: Option<RoomEventMeta>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RoomEventMeta {
    pub room_id: String,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct RoomMessagePosted {
    pub message: Message,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub room_id: String,
    #[serde(default)]
    pub actor_id: String,
    pub body: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CallToken {
    pub token: String,
    pub e2ee_key: String,
    pub call_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(truncate("héllo", 4), "hél");
        assert_eq!(truncate("世界", 4), "世");
    }

    #[test]
    fn test_new_client_trims_trailing_slash() {
        let c = Client::new("https://chat.example.com/", "tok_abc");
        assert_eq!(c.base_url(), "https://chat.example.com");
    }

    #[test]
    fn test_error_helpers() {
        let err = Error::Rpc(RpcError {
            status_code: StatusCode::FORBIDDEN,
            url: "/x".to_owned(),
            body: "Not a member of this room".to_owned(),
        });
        assert!(is_not_member_error(&err));
        assert!(!is_permission_denied_error(&err));

        let err = Error::Rpc(RpcError {
            status_code: StatusCode::FORBIDDEN,
            url: "/x".to_owned(),
            body: "Permission Denied".to_owned(),
        });
        assert!(is_permission_denied_error(&err));
        assert!(!is_not_member_error(&err));
    }

    #[tokio::test]
    async fn test_get_viewer_auth_header() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/api/connect/chatto.api.v1.ViewerService/GetViewer")
            .match_header("content-type", "application/json")
            .match_header("authorization", "Bearer tok_abc")
            .with_status(200)
            .with_body(r#"{"user":{"profile":{"id":"usr_123"}}}"#)
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok_abc");
        let id = c.get_viewer().await.unwrap();
        assert_eq!(id, "usr_123");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_no_auth_when_token_empty() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/api/connect/chatto.api.v1.ViewerService/GetViewer")
            .match_header("authorization", Matcher::Missing)
            .with_status(200)
            .with_body(r#"{"user":{"profile":{"id":"usr_1"}}}"#)
            .create_async()
            .await;

        let c = Client::new(&server.url(), "");
        let _ = c.get_viewer().await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_rpc_error_status() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/api/connect/chatto.api.v1.ViewerService/GetViewer")
            .with_status(403)
            .with_body("forbidden")
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        let err = c.get_viewer().await.unwrap_err();
        match err {
            Error::Rpc(rpc) => {
                assert_eq!(rpc.status_code, StatusCode::FORBIDDEN);
                assert!(rpc.body.contains("forbidden"));
            }
            other => panic!("expected rpc error, got: {other}"),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_unmarshal_error() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/api/connect/chatto.api.v1.ViewerService/GetViewer")
            .with_status(200)
            .with_body("not json")
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        let err = c.get_viewer().await.unwrap_err();
        match err {
            Error::Unmarshal { .. } => {}
            other => panic!("expected unmarshal error, got: {other}"),
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_room_events_with_cursor() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.RoomService/GetRoomEvents",
            )
            .match_body(Matcher::JsonString(
                r#"{"after":"c1","limit":10,"roomId":"room1"}"#.to_owned(),
            ))
            .with_status(200)
            .with_body(r#"{"page":{"events":[{"id":"evt1"}],"endCursor":"c2"}}"#)
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        let resp = c.get_room_events("room1", "c1", 10).await.unwrap();
        assert_eq!(resp.page.unwrap().end_cursor, "c2");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_room_events_no_cursor() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.RoomService/GetRoomEvents",
            )
            .match_body(Matcher::JsonString(
                r#"{"limit":10,"roomId":"room1"}"#.to_owned(),
            ))
            .with_status(200)
            .with_body(r#"{"page":{"events":[],"endCursor":"c1"}}"#)
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        let resp = c.get_room_events("room1", "", 10).await.unwrap();
        assert_eq!(resp.page.unwrap().end_cursor, "c1");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_voice_methods() {
        let mut server = Server::new_async().await;

        let join = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.VoiceCallService/JoinCall",
            )
            .with_status(200)
            .with_body(r#"{"joined":true}"#)
            .create_async()
            .await;

        let token = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.VoiceCallService/CreateCallToken",
            )
            .with_status(200)
            .with_body(r#"{"token":"jwt_token","e2eeKey":"key","callId":"call_1"}"#)
            .create_async()
            .await;

        let leave = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.VoiceCallService/LeaveCall",
            )
            .with_status(200)
            .with_body(r#"{"left":true}"#)
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        assert!(c.join_call("room1").await.unwrap());
        let tk = c.create_call_token("room1").await.unwrap();
        assert_eq!(tk.token, "jwt_token");
        assert_eq!(tk.e2ee_key, "key");
        assert_eq!(tk.call_id, "call_1");
        assert!(c.leave_call("room1").await.unwrap());

        join.assert_async().await;
        token.assert_async().await;
        leave.assert_async().await;
    }

    #[tokio::test]
    async fn test_upload_avatar_targets_user_service_with_user_id() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.UserService/UploadAvatar",
            )
            .match_header("content-type", "application/proto")
            .match_body(Matcher::from(vec![
                // UploadAvatarRequest.image = 4 (ImageUpload)
                0x22, 0x05, //
                // ImageUpload.image = 1 (bytes) = b"IMG"
                0x0A, 0x03, b'I', b'M', b'G', //
                // UploadAvatarRequest.user_id = 5 (string) = b"usr_1"
                0x2A, 0x05, b'u', b's', b'r', b'_', b'1',
            ]))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let c = Client::new(&server.url(), "cht_BK_test");
        c.upload_avatar("usr_1", b"IMG").await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_message_and_presence_methods() {
        let mut server = Server::new_async().await;

        let create_msg = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.MessageService/CreateMessage",
            )
            .match_body(Matcher::JsonString(
                r#"{"body":"hello","roomId":"room1"}"#.to_owned(),
            ))
            .with_status(200)
            .with_body("null")
            .create_async()
            .await;

        let presence = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.MyAccountService/SetPresence",
            )
            .match_body(Matcher::JsonString(
                r#"{"status":"ONLINE","userSelected":true}"#.to_owned(),
            ))
            .with_status(200)
            .with_body(r#"{"status":"PRESENCE_STATUS_ONLINE"}"#)
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        c.create_message("room1", "hello").await.unwrap();
        c.set_presence("ONLINE", true).await.unwrap();

        create_msg.assert_async().await;
        presence.assert_async().await;
    }

    fn varint(value: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_varint(&mut buf, value);
        buf
    }

    #[test]
    fn test_encode_varint_values() {
        assert_eq!(varint(0), [0x00]);
        assert_eq!(varint(1), [0x01]);
        assert_eq!(varint(127), [0x7F]);
        assert_eq!(varint(128), [0x80, 0x01]);
        assert_eq!(varint(300), [0xAC, 0x02]);
        assert_eq!(varint(16_384), [0x80, 0x80, 0x01]);
        let mut max = vec![0xFFu8; 9];
        max.push(0x01);
        assert_eq!(varint(u64::MAX), max);
    }

    #[test]
    fn test_encode_varint_appends() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 2);
        encode_varint(&mut buf, 128);
        assert_eq!(buf, vec![0x02, 0x80, 0x01]);
    }

    #[test]
    fn test_truncate_more_edges() {
        // Exact fit returns the whole string untouched.
        assert_eq!(truncate("hello", 5), "hello");
        // Cutting in the middle of a multibyte char backs off to the boundary.
        assert_eq!(truncate("aé", 2), "a");
        assert_eq!(truncate("é", 1), "");
        // max_len larger than the string.
        assert_eq!(truncate("hi", 100), "hi");
    }

    #[test]
    fn test_error_helpers_variants() {
        // not_found status codes also mean "not a member".
        let err = Error::Rpc(RpcError {
            status_code: StatusCode::NOT_FOUND,
            url: "/x".to_owned(),
            body: "rpc error: code not_found".to_owned(),
        });
        assert!(is_not_member_error(&err));
        assert!(!is_permission_denied_error(&err));

        // permission_denied code form.
        let err = Error::Rpc(RpcError {
            status_code: StatusCode::FORBIDDEN,
            url: "/x".to_owned(),
            body: "code: permission_denied".to_owned(),
        });
        assert!(is_permission_denied_error(&err));

        // Case-insensitive matching of the human message.
        let err = Error::Rpc(RpcError {
            status_code: StatusCode::FORBIDDEN,
            url: "/x".to_owned(),
            body: "Not A Member Of This Room".to_owned(),
        });
        assert!(is_not_member_error(&err));
    }

    #[test]
    fn test_helpers_false_on_non_rpc_errors() {
        // Unmarshal is not an Rpc variant: the helpers must say no.
        let err = Error::Unmarshal {
            status_code: StatusCode::OK,
            url: "/x".to_owned(),
            source: serde_json::from_str::<u32>("not a number").unwrap_err(),
            body: "not a number".to_owned(),
        };
        assert!(!is_not_member_error(&err));
        assert!(!is_permission_denied_error(&err));
    }

    #[test]
    fn test_rpc_error_display_includes_context() {
        let err = Error::Rpc(RpcError {
            status_code: StatusCode::FORBIDDEN,
            url: "https://chat.example.com/api/connect/X/Y".to_owned(),
            body: "boom".to_owned(),
        });
        let text = err.to_string();
        assert!(text.contains("403"), "got {text}");
        assert!(text.contains("https://chat.example.com"), "got {text}");
        assert!(text.contains("boom"), "got {text}");
    }

    #[test]
    fn test_room_event_serde_defaults() {
        // Bare events from the poll API must deserialize with everything
        // else defaulted.
        let event: RoomTimelineEvent = serde_json::from_str(r#"{"id":"e1"}"#).unwrap();
        assert_eq!(event.id, "e1");
        assert_eq!(event.actor_id, "");
        assert!(event.message_posted.is_none());
        assert!(event.room_created.is_none());
        assert!(event.user_joined_room.is_none());

        // camelCase messagePosted payload.
        let event: RoomTimelineEvent = serde_json::from_str(
            r#"{"id":"e2","messagePosted":{"message":{"id":"m1","roomId":"r1","actorId":"a1","body":"hi"}}}"#,
        )
        .unwrap();
        let posted = event.message_posted.unwrap();
        assert_eq!(posted.message.body.as_deref(), Some("hi"));
        assert_eq!(posted.message.room_id, "r1");

        // A null body deserializes to None.
        let event: RoomTimelineEvent =
            serde_json::from_str(r#"{"messagePosted":{"message":{"body":null}}}"#).unwrap();
        assert!(event.message_posted.unwrap().message.body.is_none());
    }

    #[test]
    fn test_page_and_profile_serde() {
        let page: RoomTimelinePage = serde_json::from_str(
            r#"{"startCursor":"s","endCursor":"e","hasNewer":true,"includes":{"users":{"u1":{"id":"u1","login":"tidal_bot"}}}}"#,
        )
        .unwrap();
        assert_eq!(page.start_cursor, "s");
        assert!(page.has_newer);
        assert!(!page.has_older);
        assert!(page.events.is_empty());
        let users = &page.includes.as_ref().unwrap().users;
        assert_eq!(users["u1"].login.as_deref(), Some("tidal_bot"));
        assert_eq!(users["u1"].display_name, None);

        // A totally empty page still parses (all fields defaulted).
        let page: RoomTimelinePage = serde_json::from_str("{}").unwrap();
        assert_eq!(page.end_cursor, "");
    }

    #[tokio::test]
    async fn test_join_and_leave_call_report_false() {
        let mut server = Server::new_async().await;
        let _join = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.VoiceCallService/JoinCall",
            )
            .with_status(200)
            .with_body(r#"{"joined":false}"#)
            .create_async()
            .await;
        let _leave = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.VoiceCallService/LeaveCall",
            )
            .with_status(200)
            .with_body(r#"{"left":false}"#)
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        assert!(!c.join_call("room1").await.unwrap());
        assert!(!c.leave_call("room1").await.unwrap());
    }

    #[tokio::test]
    async fn test_create_call_token_missing_field_is_unmarshal_error() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.VoiceCallService/CreateCallToken",
            )
            .with_status(200)
            // Missing e2eeKey and callId, which are required.
            .with_body(r#"{"token":"only-token"}"#)
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        let err = c.create_call_token("room1").await.unwrap_err();
        assert!(
            matches!(err, Error::Unmarshal { .. }),
            "expected unmarshal, got {err:?}"
        );
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_profile_reads_camel_case_fields() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/api/connect/chatto.api.v1.ViewerService/GetViewer")
            .expect(2)
            .with_status(200)
            .with_body(
                r#"{"user":{"profile":{"id":"usr_9","login":"tidal_bot","displayName":"Tidal Bot","avatarUrl":"/a/b.png"}}}"#,
            )
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        let profile = c.get_profile().await.unwrap();
        assert_eq!(profile.id, "usr_9");
        assert_eq!(profile.login.as_deref(), Some("tidal_bot"));
        assert_eq!(profile.display_name.as_deref(), Some("Tidal Bot"));
        assert_eq!(profile.avatar_url.as_deref(), Some("/a/b.png"));

        // get_viewer is a shorthand for profile.id.
        assert_eq!(c.get_viewer().await.unwrap(), "usr_9");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_membership_rpc_requests() {
        let mut server = Server::new_async().await;
        let add = server
            .mock("POST", "/api/connect/chatto.api.v1.RoomService/AddMember")
            .match_body(Matcher::JsonString(
                r#"{"roomId":"r1","userId":"usr_2"}"#.to_owned(),
            ))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        let join = server
            .mock("POST", "/api/connect/chatto.api.v1.RoomService/JoinRoom")
            .match_body(Matcher::JsonString(r#"{"roomId":"r1"}"#.to_owned()))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        c.add_member("r1", "usr_2").await.unwrap();
        c.join_room("r1").await.unwrap();
        add.assert_async().await;
        join.assert_async().await;
    }

    #[tokio::test]
    async fn test_empty_success_body_is_treated_as_null() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.MessageService/CreateMessage",
            )
            .with_status(200)
            .with_body("")
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        c.create_message("room1", "hello").await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_upload_avatar_empty_image_still_frames_request() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "POST",
                "/api/connect/chatto.api.v1.UserService/UploadAvatar",
            )
            .match_header("connect-protocol-version", "1")
            .match_body(Matcher::from(vec![
                0x22, 0x02, // image field, 2 bytes of inner message
                0x0A, 0x00, // ImageUpload.image = bytes of length 0
                0x2A, 0x01, b'u', // user_id = "u"
            ]))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        c.upload_avatar("u", b"").await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_large_error_body_is_truncated() {
        let mut server = Server::new_async().await;
        let huge = "x".repeat(600);
        let mock = server
            .mock("POST", "/api/connect/chatto.api.v1.ViewerService/GetViewer")
            .with_status(500)
            .with_body(&huge)
            .create_async()
            .await;

        let c = Client::new(&server.url(), "tok");
        let err = c.get_viewer().await.unwrap_err();
        match err {
            Error::Rpc(rpc) => assert_eq!(rpc.body.len(), 500),
            other => panic!("expected rpc error, got {other:?}"),
        }
        mock.assert_async().await;
    }
}
