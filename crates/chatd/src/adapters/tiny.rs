//! A Tiny server the owner already runs. One account lives in this network's keychain item.
//! Sign-in is the sheet's (`signin`), never `tinyclient-login`. This slice has no conversations,
//! calls, or media, and does not load a WebRTC stack. A signed-in account can list members,
//! invite, change a role, remove a member, and read the audit.

pub mod signin;

use super::{serve_commands, Command, Context, Signed};
use crate::session;
use protocol::{
    tiny_can, AccountKey, ErrorCode, Link, Network, Response, TinyAudit, TinyCode, TinyCommunity,
    TinyInvite, TinyMember,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use ureq::tls::{TlsConfig, TlsProvider};

/// How long one request may take. A redirect is not followed, so this is one host.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Saved {
    base_url: String,
    token: String,
    user_id: String,
    email: String,
    display_name: String,
    role: String,
}

struct Me {
    display_name: String,
    email: String,
}

enum Fault {
    Unauthorized,
    Unreachable,
    /// The server asked for a pause. The password was not checked.
    SlowDown,
    /// The server answered, and the answer was a failure of its own.
    Unavailable,
    BadAnswer,
    /// Words for the sheet, already chosen. Not a password failure.
    Words(&'static str),
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

pub(crate) fn parse(session: &str) -> Result<Saved, String> {
    let saved: Saved = serde_json::from_str(session)
        .map_err(|_| "the Tiny server session is unreadable".to_owned())?;
    if saved.base_url.is_empty() || saved.token.is_empty() || !safe_id(&saved.user_id) {
        return Err("the Tiny server session is incomplete".to_owned());
    }
    if !matches!(saved.role.as_str(), "owner" | "admin" | "member") {
        return Err("the Tiny server session has an unknown role".to_owned());
    }
    Ok(saved)
}

pub fn supported() -> bool {
    true
}

pub fn accounts(session: &str) -> Result<Vec<Signed>, String> {
    let saved = parse(session)?;
    Ok(vec![Signed {
        account: saved.user_id.clone(),
        name: saved.display_name,
        session: session.to_owned(),
    }])
}

pub fn without(session: &str, account: &str) -> Option<String> {
    match parse(session) {
        Ok(saved) if saved.user_id == account => None,
        _ => Some(session.to_owned()),
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .build(),
        )
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .user_agent("tinyclient")
        .build()
        .into()
}

/// One request. A 3xx is a failure: the password and the bearer stay on the host that was asked.
fn call(
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Result<(u16, Value), Fault> {
    let agent = agent();
    let bearer = token.map(|token| format!("Bearer {token}"));
    let sent = match method {
        "GET" => {
            let mut request = agent.get(url);
            if let Some(bearer) = bearer.as_deref() {
                request = request.header("Authorization", bearer);
            }
            request.call()
        }
        "DELETE" => {
            let mut request = agent.delete(url);
            if let Some(bearer) = bearer.as_deref() {
                request = request.header("Authorization", bearer);
            }
            request.call()
        }
        "POST" => {
            let mut request = agent.post(url);
            if let Some(bearer) = bearer.as_deref() {
                request = request.header("Authorization", bearer);
            }
            match body {
                Some(body) => request.send_json(body),
                None => request.send_empty(),
            }
        }
        "PATCH" => {
            let mut request = agent.patch(url);
            if let Some(bearer) = bearer.as_deref() {
                request = request.header("Authorization", bearer);
            }
            match body {
                Some(body) => request.send_json(body),
                None => request.send_empty(),
            }
        }
        _ => return Err(Fault::Unreachable),
    };
    let mut response = sent.map_err(|_| Fault::Unreachable)?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(Fault::Unreachable);
    }
    if status == 204 {
        return Ok((status, Value::Null));
    }
    // The body is not logged: a 200 may hold the bearer.
    let parsed = response.body_mut().read_json().unwrap_or(Value::Null);
    Ok((status, parsed))
}

fn endpoint(base: &str, path: &str) -> String {
    format!("{base}{path}")
}

fn post_session(base: &str, email: &str, password: &str) -> Result<Saved, Fault> {
    let (status, body) = call(
        "POST",
        &endpoint(base, "/v1/session"),
        None,
        Some(serde_json::json!({ "email": email, "password": password })),
    )?;
    match status {
        200 => saved_from(base, body),
        401 => Err(Fault::Unauthorized),
        429 => Err(Fault::SlowDown),
        500..=599 => Err(Fault::Unavailable),
        _ => Err(Fault::Unreachable),
    }
}

fn saved_from(base: &str, body: Value) -> Result<Saved, Fault> {
    let answer: SessionAnswer = serde_json::from_value(body).map_err(|_| Fault::BadAnswer)?;
    if answer.token.is_empty()
        || !safe_id(&answer.user_id)
        || !matches!(answer.role.as_str(), "owner" | "admin" | "member")
    {
        return Err(Fault::BadAnswer);
    }
    Ok(Saved {
        base_url: base.to_owned(),
        token: answer.token,
        user_id: answer.user_id,
        email: answer.email,
        display_name: answer.display_name,
        role: answer.role,
    })
}

/// Joining with a code. The same session shape as `post_session`. No bearer is sent.
fn post_accept(
    base: &str,
    token: &str,
    email: &str,
    password: &str,
    display_name: &str,
) -> Result<Saved, Fault> {
    let (status, body) = call(
        "POST",
        &endpoint(base, "/v1/invites/accept"),
        None,
        Some(serde_json::json!({
            "token": token,
            "email": email,
            "password": password,
            "display_name": display_name,
        })),
    )?;
    match status {
        200 => saved_from(base, body),
        401 => Err(Fault::Unauthorized),
        429 => Err(Fault::SlowDown),
        500..=599 => Err(Fault::Unavailable),
        _ => Err(Fault::Words(explain(status, &body, Missing::Code))),
    }
}

#[derive(Deserialize)]
struct SessionAnswer {
    token: String,
    user_id: String,
    email: String,
    display_name: String,
    role: String,
}

#[derive(Deserialize, Default)]
struct MeAnswer {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    email: String,
}

fn get_me(saved: &Saved) -> Result<Me, Fault> {
    let (status, body) = call(
        "GET",
        &endpoint(&saved.base_url, "/v1/me"),
        Some(&saved.token),
        None,
    )?;
    if status == 401 {
        return Err(Fault::Unauthorized);
    }
    if status != 200 {
        return Err(Fault::Unreachable);
    }
    let answer: MeAnswer = serde_json::from_value(body).unwrap_or_default();
    Ok(Me {
        display_name: answer.display_name,
        email: answer.email,
    })
}

fn delete_session(saved: &Saved) {
    let _ = call(
        "DELETE",
        &endpoint(&saved.base_url, "/v1/session"),
        Some(&saved.token),
        None,
    );
}

const CHECK: &str = "That didn't work. Check it and try again.";
const UNREADABLE: &str = "The server's answer could not be read.";
const UNREACHABLE: &str = "Couldn't reach that server. Check the address and your connection.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Missing {
    Member,
    Code,
    Plain,
}

fn error(code: ErrorCode, detail: impl ToString) -> Response {
    Response::Error {
        code,
        detail: detail.to_string(),
    }
}

fn refused(detail: &str) -> Response {
    error(ErrorCode::Refused, detail)
}

fn explain(status: u16, body: &Value, missing: Missing) -> &'static str {
    let code = body.get("error").and_then(Value::as_str).unwrap_or("");
    match (status, code) {
        (401, _) => "Tiny server signed you out.",
        (403, _) => "You can't do that.",
        (409, "last_owner") => "This server needs an owner. Make someone else the owner first.",
        (409, "already_member") => "That person is already on this server.",
        (409, "email_taken") => "That email is already on this server.",
        (404, _) if missing == Missing::Member => "That person is no longer on this server.",
        (404, _) if missing == Missing::Code => {
            "That code doesn't work. It may have expired or already been used."
        }
        (400, _) => CHECK,
        (429, _) => "Too many tries. Wait a minute, then try again.",
        (500..=599, _) => "The server had a problem. Try again in a moment.",
        _ => UNREACHABLE,
    }
}

/// The signed-in session for `account`, or a refusal. Nothing is asked of the server here.
fn open(account: &AccountKey) -> Result<Saved, Response> {
    if account.network != Network::Tiny {
        return Err(refused("You can't do that."));
    }
    let session = match session::read(Network::Tiny) {
        session::Stored::Found(session) => session,
        session::Stored::Absent => return Err(refused("You can't do that.")),
        session::Stored::Unreadable(why) => {
            return Err(error(
                ErrorCode::Internal,
                format!("The session could not be read from {}: {why}", session::STORE),
            ));
        }
    };
    let saved = match parse(&session) {
        Ok(saved) => saved,
        Err(_) => return Err(refused("You can't do that.")),
    };
    if saved.user_id != account.account {
        return Err(refused("You can't do that."));
    }
    Ok(saved)
}

fn asked(method: &str, saved: &Saved, path: &str, body: Option<Value>) -> Result<(u16, Value), &'static str> {
    match call(method, &endpoint(&saved.base_url, path), Some(&saved.token), body) {
        Ok(answer) => Ok(answer),
        Err(_) => Err(UNREACHABLE),
    }
}

enum Listed<T> {
    Rows(T),
    /// A 403: the caller may not see this list. The load still succeeds.
    Empty,
    Failed(&'static str),
}

fn listed<T>(saved: &Saved, path: &str, parse: impl FnOnce(Value) -> Result<T, ()>) -> Listed<T> {
    match asked("GET", saved, path, None) {
        Err(detail) => Listed::Failed(detail),
        Ok((403, _)) => Listed::Empty,
        Ok((200, body)) => match parse(body) {
            Ok(rows) => Listed::Rows(rows),
            Err(()) => Listed::Failed(UNREADABLE),
        },
        Ok((status, body)) => Listed::Failed(explain(status, &body, Missing::Plain)),
    }
}

fn members_of(saved: &Saved) -> Result<Vec<TinyMember>, &'static str> {
    match asked("GET", saved, "/v1/members", None) {
        Err(detail) => Err(detail),
        Ok((200, body)) => serde_json::from_value(body).map_err(|_| UNREADABLE),
        Ok((status, body)) => Err(explain(status, &body, Missing::Plain)),
    }
}

fn invites_of(saved: &Saved) -> Listed<Vec<TinyInvite>> {
    listed(saved, "/v1/invites", |body| serde_json::from_value(body).map_err(|_| ()))
}

#[derive(Deserialize)]
struct AuditWire {
    id: i64,
    #[serde(default)]
    actor_id: Option<String>,
    action: String,
    subject: String,
    #[serde(default)]
    detail: Option<Value>,
    created_at: String,
}

fn detail_text(detail: Option<Value>) -> String {
    match detail {
        None | Some(Value::Null) => String::new(),
        Some(value) => serde_json::to_string(&value).unwrap_or_default(),
    }
}

fn audit_of(saved: &Saved) -> Listed<Vec<TinyAudit>> {
    listed(saved, "/v1/audit?limit=50", |body| {
        let rows: Vec<AuditWire> = serde_json::from_value(body).map_err(|_| ())?;
        Ok(rows
            .into_iter()
            .map(|row| TinyAudit {
                id: row.id,
                actor_id: row.actor_id,
                action: row.action,
                subject: row.subject,
                detail: detail_text(row.detail),
                created_at: row.created_at,
            })
            .collect())
    })
}

pub(crate) fn community(account: AccountKey) -> Response {
    let saved = match open(&account) {
        Ok(saved) => saved,
        Err(error) => return error,
    };
    let members = match members_of(&saved) {
        Ok(members) => members,
        Err(detail) => return refused(detail),
    };
    // A 403 on invites or the audit is an empty list. Any other failure fails the load.
    // The role is the one that was saved, not a role from the member list.
    let invites = if tiny_can(&saved.role, "community.invites.read") {
        match invites_of(&saved) {
            Listed::Rows(rows) => rows,
            Listed::Empty => Vec::new(),
            Listed::Failed(detail) => return refused(detail),
        }
    } else {
        Vec::new()
    };
    let audit = if tiny_can(&saved.role, "community.audit.read") {
        match audit_of(&saved) {
            Listed::Rows(rows) => rows,
            Listed::Empty => Vec::new(),
            Listed::Failed(detail) => return refused(detail),
        }
    } else {
        Vec::new()
    };
    Response::TinyCommunity {
        community: TinyCommunity {
            role: saved.role,
            members,
            invites,
            audit,
        },
    }
}

pub(crate) fn invite(account: AccountKey, role: String) -> Response {
    let saved = match open(&account) {
        Ok(saved) => saved,
        Err(error) => return error,
    };
    if role != "admin" && role != "member" {
        return refused(CHECK);
    }
    match asked(
        "POST",
        &saved,
        "/v1/invites",
        Some(serde_json::json!({ "role": role })),
    ) {
        Err(detail) => refused(detail),
        Ok((201, body)) => match serde_json::from_value::<TinyCode>(body) {
            Ok(code) => Response::TinyCode { code },
            Err(_) => refused(UNREADABLE),
        },
        Ok((status, body)) => refused(explain(status, &body, Missing::Plain)),
    }
}

pub(crate) fn set_role(account: AccountKey, user_id: String, role: String) -> Response {
    let saved = match open(&account) {
        Ok(saved) => saved,
        Err(error) => return error,
    };
    if !safe_id(&user_id) || !matches!(role.as_str(), "owner" | "admin" | "member") {
        return refused(CHECK);
    }
    let path = format!("/v1/members/{user_id}");
    match asked(
        "PATCH",
        &saved,
        &path,
        Some(serde_json::json!({ "role": role })),
    ) {
        Err(detail) => refused(detail),
        Ok((200, body)) => match body.get("role").and_then(Value::as_str) {
            Some(role) if matches!(role, "owner" | "admin" | "member") => Response::Ok,
            _ => refused(UNREADABLE),
        },
        Ok((status, body)) => refused(explain(status, &body, Missing::Member)),
    }
}

pub(crate) fn remove(account: AccountKey, user_id: String) -> Response {
    let saved = match open(&account) {
        Ok(saved) => saved,
        Err(error) => return error,
    };
    if !safe_id(&user_id) {
        return refused(CHECK);
    }
    let path = format!("/v1/members/{user_id}");
    match asked("DELETE", &saved, &path, None) {
        Err(detail) => refused(detail),
        Ok((204, _)) => Response::Ok,
        Ok((status, body)) => refused(explain(status, &body, Missing::Member)),
    }
}

pub fn start(ctx: Context, session: &str, inbox: Receiver<Command>) -> Result<(), String> {
    let saved = parse(session)?;
    if saved.user_id != ctx.account.account {
        return Err("the Tiny server session is for another account".to_owned());
    }
    let (probe, probe_ctx) = (saved.clone(), ctx.clone());
    std::thread::Builder::new()
        .name("tiny-link".into())
        .spawn(move || match get_me(&probe) {
            Ok(me) => {
                let _ = probe_ctx.describe(&me.display_name, &me.email);
                probe_ctx.link(Link::Connected);
            }
            Err(Fault::Unauthorized) => probe_ctx.link(Link::Expired),
            Err(_) => {
                eprintln!(
                    "chatd: {}: could not reach the server",
                    probe_ctx.account.network.label()
                );
                probe_ctx.link(Link::Offline);
            }
        })
        .map_err(|err| err.to_string())?;
    std::thread::Builder::new()
        .name("tiny-commands".into())
        .spawn(move || {
            serve_commands(
                &ctx,
                inbox,
                Duration::from_millis(200),
                |_| {},
                |command| match command {
                    Command::SignOut(reply) => {
                        delete_session(&saved);
                        let _ = reply.send(Ok(()));
                    }
                    other => other.unsupported(&ctx),
                },
            );
        })
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn session() -> &'static str {
        r#"{"base_url":"http://127.0.0.1:8080","token":"t","user_id":"11111111-1111-1111-1111-111111111111","email":"ada@example.com","display_name":"Ada","role":"owner"}"#
    }

    #[test]
    fn a_session_is_one_account_and_a_bad_one_is_an_error() {
        let signed = accounts(session()).unwrap();
        assert_eq!(signed.len(), 1);
        assert_eq!(signed[0].account, "11111111-1111-1111-1111-111111111111");
        assert_eq!(signed[0].name, "Ada");
        assert_eq!(signed[0].session, session());
        assert!(without(session(), &signed[0].account).is_none());
        assert_eq!(
            without(session(), "someone-else").as_deref(),
            Some(session())
        );
        assert!(accounts("not json").is_err());
        assert!(accounts(r#"{"base_url":"http://127.0.0.1:8080","token":"t","user_id":"../x","email":"a@b.co","display_name":"A","role":"owner"}"#).is_err());
        let saved = parse(session()).unwrap();
        assert_eq!(saved.role, "owner");
        assert_eq!(saved.base_url, "http://127.0.0.1:8080");
        assert_eq!(saved.email, "ada@example.com");
    }

    /// Speaks the session routes on a local socket: wrong password, open, me, delete, and a
    /// redirect that must not be followed.
    #[test]
    fn session_routes_against_a_local_server() {
        let base = serve(|request| {
            let first = request.lines().next().unwrap_or("");
            if first.starts_with("POST /v1/session ") {
                if request.contains("\"password\": \"slow\"") {
                    return http(429, "Too Many Requests", r#"{"error":"slow_down"}"#);
                }
                if request.contains("\"password\": \"boom\"") {
                    return http(500, "Internal Server Error", r#"{"error":"unavailable"}"#);
                }
                if request.contains("\"password\": \"nope\"") {
                    return http(401, "Unauthorized", r#"{"error":"invalid_credentials"}"#);
                }
                return http(
                    200,
                    "OK",
                    r#"{"token":"tok_value","user_id":"11111111-1111-1111-1111-111111111111","email":"ada@example.com","display_name":"Ada","role":"owner"}"#,
                );
            }
            if first.starts_with("GET /v1/me ") {
                if request.contains("Bearer tok_value") {
                    return http(
                        200,
                        "OK",
                        r#"{"user_id":"11111111-1111-1111-1111-111111111111","email":"ada@example.com","display_name":"Ada","role":"owner"}"#,
                    );
                }
                return http(401, "Unauthorized", r#"{"error":"unauthorized"}"#);
            }
            if first.starts_with("DELETE /v1/session ") {
                return http(204, "No Content", "");
            }
            if first.starts_with("POST /v1/bounce ") {
                b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            } else {
                http(404, "Not Found", "{}")
            }
        });

        assert!(matches!(
            post_session(&base, "ada@example.com", "nope"),
            Err(Fault::Unauthorized)
        ));
        assert!(matches!(
            post_session(&base, "ada@example.com", "slow"),
            Err(Fault::SlowDown)
        ));
        assert!(matches!(
            post_session(&base, "ada@example.com", "boom"),
            Err(Fault::Unavailable)
        ));
        let saved = match post_session(&base, "ada@example.com", "correct-password") {
            Ok(saved) => saved,
            Err(_) => panic!("open failed"),
        };
        assert_eq!(saved.user_id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(saved.role, "owner");
        assert_eq!(saved.email, "ada@example.com");
        assert_eq!(saved.base_url, base);
        assert_eq!(saved.token, "tok_value");

        let me = match get_me(&saved) {
            Ok(me) => me,
            Err(_) => panic!("me failed"),
        };
        assert_eq!(me.display_name, "Ada");
        assert_eq!(me.email, "ada@example.com");
        delete_session(&saved);

        let bounced = call(
            "POST",
            &endpoint(&base, "/v1/bounce"),
            None,
            Some(serde_json::json!({"email": "ada@example.com", "password": "correct-password"})),
        );
        assert!(matches!(bounced, Err(Fault::Unreachable)));
    }

    /// Against a tinyserver that is already serving. Not part of `cargo test`: it needs
    /// `TINY_LIVE_BASE`, `TINY_LIVE_EMAIL`, and `TINY_LIVE_PASSWORD`.
    #[test]
    #[ignore]
    fn live_owner_round_trip() {
        let base = std::env::var("TINY_LIVE_BASE").expect("TINY_LIVE_BASE");
        let email = std::env::var("TINY_LIVE_EMAIL").expect("TINY_LIVE_EMAIL");
        let password = std::env::var("TINY_LIVE_PASSWORD").expect("TINY_LIVE_PASSWORD");
        let saved = match post_session(&base, &email, &password) {
            Ok(saved) => saved,
            Err(_) => panic!("open failed"),
        };
        assert_eq!(saved.role, "owner");
        assert_eq!(saved.email, email.trim().to_ascii_lowercase());
        let me = match get_me(&saved) {
            Ok(me) => me,
            Err(_) => panic!("me failed"),
        };
        assert_eq!(me.email, saved.email);
        delete_session(&saved);
        assert!(matches!(get_me(&saved), Err(Fault::Unauthorized)));
    }

    fn with_session(base: &str, role: &str, run: impl FnOnce(AccountKey)) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let name = format!("TINYCLIENT_SESSION_{}", Network::Tiny.label().to_uppercase());
        let user_id = "11111111-1111-1111-1111-111111111111";
        let session = format!(
            r#"{{"base_url":{base:?},"token":"t","user_id":{user_id:?},"email":"ada@example.com","display_name":"Ada","role":{role:?}}}"#
        );
        std::env::set_var("TINYCLIENT_DIR", std::env::temp_dir());
        std::env::set_var(&name, &session);
        match crate::session::read(Network::Tiny) {
            crate::session::Stored::Found(_) => {}
            crate::session::Stored::Absent => panic!("tiny session was absent ({name})"),
            crate::session::Stored::Unreadable(why) => panic!("tiny session was unreadable: {why}"),
        }
        run(AccountKey {
            network: Network::Tiny,
            account: user_id.to_owned(),
        });
    }

    #[test]
    fn a_member_loads_members_and_does_not_ask_for_invites_or_the_audit() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = log.clone();
        let base = serve(move |request| {
            seen.lock().unwrap().push(request.to_owned());
            let first = request.lines().next().unwrap_or("");
            if first.starts_with("GET /v1/members ") {
                return http(
                    200,
                    "OK",
                    r#"[{"user_id":"11111111-1111-1111-1111-111111111111","email":"ada@example.com","display_name":"Ada","role":"owner","created_at":"2020-01-01T00:00:00Z"}]"#,
                );
            }
            http(500, "Internal Server Error", r#"{"error":"no"}"#)
        });
        with_session(&base, "member", |account| match community(account) {
            Response::TinyCommunity { community } => {
                assert_eq!(community.role, "member");
                assert_eq!(community.members.len(), 1);
                assert_eq!(community.members[0].display_name, "Ada");
                assert_eq!(community.members[0].email, "ada@example.com");
                assert_eq!(community.members[0].role, "owner");
                assert!(community.invites.is_empty());
                assert!(community.audit.is_empty());
            }
            other => panic!("load: {other:?}"),
        });
        let lines = log.lock().unwrap();
        assert!(lines.iter().any(|line| line.contains("GET /v1/members ")));
        assert!(lines
            .iter()
            .all(|line| !line.contains("/v1/invites") && !line.contains("/v1/audit")));
    }

    #[test]
    fn a_forbidden_invite_list_still_returns_the_members() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = log.clone();
        let base = serve(move |request| {
            seen.lock().unwrap().push(request.to_owned());
            let first = request.lines().next().unwrap_or("");
            if first.starts_with("GET /v1/members ") {
                return http(
                    200,
                    "OK",
                    r#"[{"user_id":"11111111-1111-1111-1111-111111111111","email":"ada@example.com","display_name":"Ada","role":"member","created_at":"2020-01-01T00:00:00Z"}]"#,
                );
            }
            if first.starts_with("GET /v1/invites ") {
                return http(403, "Forbidden", r#"{"error":"forbidden"}"#);
            }
            if first.starts_with("GET /v1/audit") {
                return http(
                    200,
                    "OK",
                    r#"[{"id":7,"actor_id":"11111111-1111-1111-1111-111111111111","action":"member.role","subject":"22222222-2222-2222-2222-222222222222","detail":{"to":"admin"},"created_at":"2020-01-02T00:00:00Z"},{"id":8,"actor_id":null,"action":"member.remove","subject":"x","detail":null,"created_at":"2020-01-03T00:00:00Z"}]"#,
                );
            }
            http(500, "Internal Server Error", r#"{"error":"no"}"#)
        });
        with_session(&base, "admin", |account| match community(account) {
            Response::TinyCommunity { community } => {
                assert_eq!(community.role, "admin");
                assert_eq!(community.members.len(), 1);
                assert_eq!(community.members[0].role, "member");
                assert!(community.invites.is_empty());
                assert_eq!(community.audit.len(), 2);
                assert_eq!(community.audit[0].id, 7);
                assert_eq!(community.audit[0].detail, r#"{"to":"admin"}"#);
                assert_eq!(community.audit[1].detail, "");
                assert_eq!(community.audit[1].actor_id, None);
            }
            other => panic!("load: {other:?}"),
        });
        let lines = log.lock().unwrap();
        assert!(lines.iter().any(|line| line.contains("GET /v1/members ")));
        assert!(lines.iter().any(|line| line.contains("GET /v1/invites ")));
        assert!(lines.iter().any(|line| line.contains("GET /v1/audit?limit=50")));
    }

    #[test]
    fn an_invite_a_role_and_a_removal_use_the_member_routes() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = log.clone();
        let base = serve(move |request| {
            seen.lock().unwrap().push(request.to_owned());
            let first = request.lines().next().unwrap_or("");
            if first.starts_with("POST /v1/invites ") {
                return http(
                    201,
                    "Created",
                    r#"{"id":"inv1","token":"invite-token","role":"member","expires_at":"2026-01-01T00:00:00Z"}"#,
                );
            }
            if first.starts_with("PATCH /v1/members/last-owner ") {
                return http(409, "Conflict", r#"{"error":"last_owner"}"#);
            }
            if first.starts_with("PATCH /v1/members/") {
                return http(200, "OK", r#"{"role":"admin"}"#);
            }
            if first.starts_with("DELETE /v1/members/") {
                return http(204, "No Content", "");
            }
            http(404, "Not Found", r#"{"error":"missing"}"#)
        });
        let refused = |detail: &str| Response::Error {
            code: ErrorCode::Refused,
            detail: detail.to_owned(),
        };
        with_session(&base, "owner", |account| {
            let quiet = || log.lock().unwrap().len();
            let before = quiet();
            assert_eq!(
                set_role(account.clone(), "not a user".into(), "admin".into()),
                refused("That didn't work. Check it and try again.")
            );
            assert_eq!(quiet(), before);
            assert_eq!(
                invite(account.clone(), "owner".into()),
                refused("That didn't work. Check it and try again.")
            );
            assert_eq!(quiet(), before);
            match invite(account.clone(), "member".into()) {
                Response::TinyCode { code } => {
                    assert_eq!(code.token, "invite-token");
                    assert_eq!(code.id, "inv1");
                    assert_eq!(code.role, "member");
                    assert_eq!(code.expires_at, "2026-01-01T00:00:00Z");
                }
                other => panic!("invite: {other:?}"),
            }
            let posted = log.lock().unwrap().last().cloned().unwrap_or_default();
            assert!(posted.lines().next().unwrap_or("").starts_with("POST /v1/invites "));
            assert_eq!(json_body(&posted)["role"], "member");
            let id = "22222222-2222-2222-2222-222222222222";
            assert_eq!(set_role(account.clone(), id.into(), "admin".into()), Response::Ok);
            let patched = log.lock().unwrap().last().cloned().unwrap_or_default();
            assert!(patched
                .lines()
                .next()
                .unwrap_or("")
                .starts_with(&format!("PATCH /v1/members/{id} ")));
            assert_eq!(json_body(&patched)["role"], "admin");
            assert_eq!(
                set_role(account.clone(), "last-owner".into(), "member".into()),
                refused("This server needs an owner. Make someone else the owner first.")
            );
            assert_eq!(remove(account.clone(), id.into()), Response::Ok);
            let deleted = log.lock().unwrap().last().cloned().unwrap_or_default();
            assert!(deleted.contains(&format!("DELETE /v1/members/{id} ")), "{deleted}");
            let before = quiet();
            assert_eq!(
                remove(account, "a/b".into()),
                refused("That didn't work. Check it and try again.")
            );
            assert_eq!(quiet(), before);
        });
    }

    fn json_body(request: &str) -> serde_json::Value {
        let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
        serde_json::from_str(body).unwrap_or_else(|err| panic!("body {err}"))
    }

    pub(crate) fn http(status: u16, reason: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    pub(crate) fn serve(handler: impl Fn(&str) -> Vec<u8> + Send + 'static) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            listener.set_nonblocking(true).expect("nonblocking");
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                        let mut got = Vec::new();
                        let mut buf = [0u8; 2048];
                        let header_end = loop {
                            if let Some(at) = got.windows(4).position(|window| window == b"\r\n\r\n") {
                                break Some(at + 4);
                            }
                            match std::io::Read::read(&mut stream, &mut buf) {
                                Ok(0) | Err(_) => break None,
                                Ok(read) => got.extend_from_slice(&buf[..read]),
                            }
                        };
                        if let Some(header_end) = header_end {
                            let head = String::from_utf8_lossy(&got[..header_end]);
                            let length = head.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                if name.eq_ignore_ascii_case("content-length") {
                                    value.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            });
                            if let Some(length) = length {
                                while got.len() < header_end + length {
                                    match std::io::Read::read(&mut stream, &mut buf) {
                                        Ok(0) | Err(_) => break,
                                        Ok(read) => got.extend_from_slice(&buf[..read]),
                                    }
                                }
                            }
                        }
                        let request = String::from_utf8_lossy(&got).into_owned();
                        let _ = std::io::Write::write_all(&mut stream, &handler(&request));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        format!("http://127.0.0.1:{port}")
    }
}
