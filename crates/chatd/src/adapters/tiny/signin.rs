//! Signing in to a Tiny server the owner already runs: the address, then the email, then the
//! password. Nothing is kept until `POST /v1/session` answers 200. The password and the bearer
//! are not logged. One sign-in at a time, in memory; closing the sheet drops it.

use super::{post_accept, post_session, Fault};
use crate::hub::Hub;
use protocol::{AccountKey, Network, SignInInput, SignInProblem, SignInStep};
use std::sync::{Arc, Mutex, MutexGuard};

/// Keeps a finished session and starts its account.
pub type Adopt = Box<dyn Fn(&str) -> Result<Vec<AccountKey>, String> + Send + Sync>;

struct Flow {
    server: String,
    email: String,
    code: String,
    display_name: String,
    joining: bool,
    step: SignInStep,
    adopt: Adopt,
}

static FLOW: Mutex<Option<Flow>> = Mutex::new(None);

fn current() -> MutexGuard<'static, Option<Flow>> {
    FLOW.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An http or https origin. A host with no scheme is https, except a machine on this computer,
/// which is http. A path, query, or fragment is left off. A user in the address is refused.
pub(crate) fn base_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().any(char::is_whitespace) {
        return None;
    }
    let (given, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, raw),
    };
    if given.is_some_and(|scheme| {
        !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https")
    }) {
        return None;
    }
    // A user in the address would carry a password. A backslash is not part of a host.
    if rest.contains(['@', '\\']) || rest.is_empty() {
        return None;
    }
    let rest = rest.split_once('#').map(|(rest, _)| rest).unwrap_or(rest);
    let rest = rest.split_once('?').map(|(rest, _)| rest).unwrap_or(rest);
    let rest = rest.split_once('/').map(|(rest, _)| rest).unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    let ok = if let Some(v6) = rest.strip_prefix('[') {
        let (addr, after) = v6.split_once(']')?;
        !addr.is_empty()
            && addr
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
            && match after {
                "" => true,
                port => port_ok(port.strip_prefix(':')?),
            }
    } else {
        match rest.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
                host_ok(host) && port_ok(port)
            }
            Some(_) => false,
            None => host_ok(rest),
        }
    };
    if !ok {
        return None;
    }
    let scheme = match given {
        Some(scheme) => scheme.to_ascii_lowercase(),
        None if local_origin(rest) => "http".to_owned(),
        None => "https".to_owned(),
    };
    Some(format!("{scheme}://{rest}"))
}

fn local_origin(rest: &str) -> bool {
    let host = if let Some(v6) = rest.strip_prefix('[') {
        v6.split_once(']').map(|(addr, _)| addr).unwrap_or("")
    } else {
        match rest.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
            _ => rest,
        }
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host == "0.0.0.0"
}

fn host_ok(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with(['.', '-'])
        && !host.ends_with('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

fn port_ok(port: &str) -> bool {
    port.parse::<u16>().is_ok_and(|port| port > 0)
}

/// One `@`, something on each side, and no whitespace.
pub(crate) fn email_ok(raw: &str) -> bool {
    let email = raw.trim();
    if email.is_empty() || email.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && !domain.contains('@')
}

/// Begins a sign-in, or with `input` answers the step on show.
pub fn step(
    _hub: &Arc<Hub>,
    input: Option<SignInInput>,
    adopt: Adopt,
) -> Result<SignInStep, String> {
    let mut held = current();
    let Some(input) = input else {
        if held.is_none() {
            *held = Some(Flow {
                server: String::new(),
                email: String::new(),
                code: String::new(),
                display_name: String::new(),
                joining: false,
                step: SignInStep::Server { problem: None },
                adopt,
            });
        }
        return Ok(held
            .as_ref()
            .map(|flow| flow.step.clone())
            .unwrap_or(SignInStep::Server { problem: None }));
    };
    let Some(flow) = held.as_mut() else {
        return Ok(SignInStep::Failed {
            reason: "That sign-in is over. Start again.".to_owned(),
        });
    };
    let next = flow.answer(input);
    if matches!(next, SignInStep::Done { .. } | SignInStep::Failed { .. }) {
        *held = None;
    }
    Ok(next)
}

/// The user closed the sheet. Nothing was kept.
pub fn cancel() {
    *current() = None;
}

impl Flow {
    fn answer(&mut self, input: SignInInput) -> SignInStep {
        let joining = self.joining;
        let next = match (&self.step, input) {
            (SignInStep::Server { .. }, SignInInput::Server { url }) => match base_url(&url) {
                Some(server) => {
                    self.server = server.clone();
                    self.joining = false;
                    SignInStep::Email {
                        server,
                        problem: None,
                    }
                }
                None => SignInStep::Server {
                    problem: Some(SignInProblem::UrlInvalid),
                },
            },
            (SignInStep::Server { .. }, SignInInput::HaveCode { url }) => match base_url(&url) {
                Some(server) => {
                    self.server = server.clone();
                    self.joining = true;
                    SignInStep::Invite {
                        server,
                        problem: None,
                    }
                }
                None => SignInStep::Server {
                    problem: Some(SignInProblem::UrlInvalid),
                },
            },
            (SignInStep::Invite { .. }, SignInInput::Invite { code }) => {
                let code = code.trim();
                if code.is_empty() {
                    SignInStep::Invite {
                        server: self.server.clone(),
                        problem: Some(SignInProblem::Other {
                            reason: "Paste the code you were given.".to_owned(),
                        }),
                    }
                } else {
                    self.code = code.to_owned();
                    SignInStep::Name {
                        server: self.server.clone(),
                        problem: None,
                    }
                }
            }
            (SignInStep::Invite { .. }, SignInInput::Back) => SignInStep::Server { problem: None },
            (SignInStep::Name { .. }, SignInInput::Name { name }) => {
                let name = name.trim();
                if name.is_empty() || name.chars().count() > 80 {
                    SignInStep::Name {
                        server: self.server.clone(),
                        problem: Some(SignInProblem::Other {
                            reason: "Type the name people should see.".to_owned(),
                        }),
                    }
                } else {
                    self.display_name = name.to_owned();
                    SignInStep::Email {
                        server: self.server.clone(),
                        problem: None,
                    }
                }
            }
            (SignInStep::Name { .. }, SignInInput::Back) => SignInStep::Invite {
                server: self.server.clone(),
                problem: None,
            },
            (SignInStep::Email { .. }, SignInInput::Email { email }) => {
                if email_ok(&email) {
                    self.email = email.trim().to_owned();
                    SignInStep::Password {
                        hint: String::new(),
                        problem: None,
                    }
                } else {
                    SignInStep::Email {
                        server: self.server.clone(),
                        problem: Some(SignInProblem::EmailInvalid),
                    }
                }
            }
            (SignInStep::Email { .. }, SignInInput::Back) if joining => SignInStep::Name {
                server: self.server.clone(),
                problem: None,
            },
            (SignInStep::Email { .. }, SignInInput::Back) => SignInStep::Server { problem: None },
            (SignInStep::Password { .. }, SignInInput::Back) => SignInStep::Email {
                server: self.server.clone(),
                problem: None,
            },
            (SignInStep::Password { .. }, SignInInput::Password { password }) => {
                if joining {
                    self.accept(&password)
                } else {
                    self.submit(&password)
                }
            }
            _ => self.step.clone(),
        };
        self.step = next.clone();
        next
    }

    fn submit(&self, password: &str) -> SignInStep {
        let again = |problem| SignInStep::Password {
            hint: String::new(),
            problem: Some(problem),
        };
        match post_session(&self.server, &self.email, password) {
            Ok(saved) => self.kept(saved),
            Err(fault) => match password_outcome(fault) {
                Ok(problem) => again(problem),
                Err(reason) => SignInStep::Failed {
                    reason: reason.to_owned(),
                },
            },
        }
    }

    fn accept(&self, password: &str) -> SignInStep {
        let again = |problem| SignInStep::Password {
            hint: String::new(),
            problem: Some(problem),
        };
        let length = password.chars().count();
        if !(12..=128).contains(&length) {
            return again(SignInProblem::Other {
                reason: "Use 12 to 128 characters.".to_owned(),
            });
        }
        match post_accept(
            &self.server,
            &self.code,
            &self.email,
            password,
            &self.display_name,
        ) {
            Ok(saved) => self.kept(saved),
            Err(fault) => match password_outcome(fault) {
                Ok(problem) => again(problem),
                Err(reason) => SignInStep::Failed {
                    reason: reason.to_owned(),
                },
            },
        }
    }

    fn kept(&self, saved: super::Saved) -> SignInStep {
        let account = saved.user_id.clone();
        let session = match serde_json::to_string(&saved) {
            Ok(session) => session,
            Err(_) => {
                return SignInStep::Failed {
                    reason: "The server's answer could not be kept.".to_owned(),
                }
            }
        };
        match (self.adopt)(&session) {
            Ok(_) => SignInStep::Done {
                account: Some(AccountKey {
                    network: Network::Tiny,
                    account,
                }),
            },
            Err(_) => SignInStep::Failed {
                reason: "The sign-in could not be kept.".to_owned(),
            },
        }
    }
}

/// A failed password step stays on that step. `Err` ends the sheet: the answer could not be read.
fn password_outcome(fault: Fault) -> Result<SignInProblem, &'static str> {
    match fault {
        Fault::Unauthorized => Ok(SignInProblem::PasswordInvalid),
        Fault::SlowDown => Ok(SignInProblem::TooMany {
            until: unix_now() + 60,
        }),
        Fault::Unavailable => Ok(SignInProblem::Other {
            reason: "The server had a problem. Try again in a moment.".to_owned(),
        }),
        Fault::Unreachable => Ok(SignInProblem::Unreachable),
        Fault::BadAnswer => Err("The server's answer could not be read."),
        Fault::Words(reason) => Ok(SignInProblem::Other {
            reason: reason.to_owned(),
        }),
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn hub() -> Arc<Hub> {
        Arc::new(Hub::new(
            Store::open(std::path::Path::new(":memory:")).expect("a store in memory"),
        ))
    }

    fn adopt() -> Adopt {
        Box::new(|_session| Ok(Vec::new()))
    }

    fn one_sheet() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn a_bad_address_stays_on_the_server_step_and_a_bad_email_stays_on_email() {
        let _one = one_sheet();
        cancel();
        let hub = hub();
        assert_eq!(
            step(&hub, None, adopt()).unwrap(),
            SignInStep::Server { problem: None }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Server {
                    url: "not a url".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Server {
                problem: Some(SignInProblem::UrlInvalid)
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Server {
                    url: "http://user:pw@127.0.0.1:8080".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Server {
                problem: Some(SignInProblem::UrlInvalid)
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Server {
                    url: "http://127.0.0.1:8080/v1".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Email {
                server: "http://127.0.0.1:8080".into(),
                problem: None
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Email {
                    email: "not an email".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Email {
                server: "http://127.0.0.1:8080".into(),
                problem: Some(SignInProblem::EmailInvalid)
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Email {
                    email: "ada@example.com".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Password {
                hint: String::new(),
                problem: None
            }
        );
        assert_eq!(
            step(&hub, Some(SignInInput::Back), adopt()).unwrap(),
            SignInStep::Email {
                server: "http://127.0.0.1:8080".into(),
                problem: None
            }
        );
        assert_eq!(
            step(&hub, Some(SignInInput::Back), adopt()).unwrap(),
            SignInStep::Server { problem: None }
        );
        cancel();
    }

    #[test]
    fn a_slow_or_broken_server_stays_on_the_password() {
        let before = unix_now();
        let SignInProblem::TooMany { until } = password_outcome(Fault::SlowDown).unwrap() else {
            panic!("a pause");
        };
        assert!((before + 60..=before + 61).contains(&until));
        assert_eq!(
            password_outcome(Fault::Unavailable).unwrap(),
            SignInProblem::Other {
                reason: "The server had a problem. Try again in a moment.".into()
            }
        );
        assert!(matches!(
            password_outcome(Fault::Unauthorized),
            Ok(SignInProblem::PasswordInvalid)
        ));
        assert!(matches!(
            password_outcome(Fault::Unreachable),
            Ok(SignInProblem::Unreachable)
        ));
        assert_eq!(
            password_outcome(Fault::BadAnswer),
            Err("The server's answer could not be read.")
        );
    }

    #[test]
    fn the_address_is_an_origin_and_the_email_has_one_at() {
        assert_eq!(
            base_url("  https://Example.com/  ").as_deref(),
            Some("https://Example.com")
        );
        assert_eq!(
            base_url("private.tinyclient.io").as_deref(),
            Some("https://private.tinyclient.io")
        );
        assert_eq!(
            base_url("127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(base_url("localhost").as_deref(), Some("http://localhost"));
        assert_eq!(
            base_url("http://[::1]:8080").as_deref(),
            Some("http://[::1]:8080")
        );
        assert_eq!(
            base_url("http://127.0.0.1:8080?x=1").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            base_url("http://127.0.0.1:8080#top").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            base_url("https://chat.example/rooms?x=1#y").as_deref(),
            Some("https://chat.example")
        );
        assert_eq!(
            base_url("chat.example/health").as_deref(),
            Some("https://chat.example")
        );
        assert_eq!(
            base_url("localhost:8080/health").as_deref(),
            Some("http://localhost:8080")
        );
        assert_eq!(base_url("ftp://127.0.0.1:8080"), None);
        assert_eq!(base_url("http://127.0.0.1:0"), None);
        assert!(email_ok("ada@example.com"));
        assert!(email_ok(" a@b "));
        assert!(!email_ok("ada"));
        assert!(!email_ok("a@b@c"));
        assert!(!email_ok("a @b.co"));
        assert!(!email_ok("@b.co"));
        assert!(!format!(
            "{:?}",
            SignInInput::Server {
                url: "http://secret.example".into()
            }
        )
        .contains("secret"));
        assert!(!format!(
            "{:?}",
            SignInInput::Email {
                email: "ada@example.com".into()
            }
        )
        .contains("ada"));
        assert!(!format!(
            "{:?}",
            SignInInput::Password {
                password: "hunter2".into()
            }
        )
        .contains("hunter2"));
        assert!(!format!(
            "{:?}",
            SignInInput::Invite {
                code: "secret-code".into()
            }
        )
        .contains("secret"));
        assert!(!format!(
            "{:?}",
            SignInInput::Name {
                name: "Ada Lovelace".into()
            }
        )
        .contains("Ada"));
    }

    #[test]
    fn a_code_joins_from_the_same_sheet() {
        let _one = one_sheet();
        cancel();
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = hits.clone();
        let base = super::super::tests::serve(move |request| {
            seen.lock().unwrap().push(request.to_owned());
            if request.lines().next().unwrap_or("").starts_with("POST /v1/invites/accept ") {
                return super::super::tests::http(
                    200,
                    "OK",
                    r#"{"token":"tok_join","user_id":"22222222-2222-2222-2222-222222222222","email":"ada@example.com","display_name":"Ada","role":"member"}"#,
                );
            }
            super::super::tests::http(500, "Internal Server Error", r#"{"error":"no"}"#)
        });
        let kept = std::sync::Arc::new(std::sync::Mutex::new(None));
        let kept_step = kept.clone();
        let hub = hub();
        assert_eq!(
            step(
                &hub,
                None,
                Box::new(move |session| {
                    *kept_step.lock().unwrap() = Some(session.to_owned());
                    Ok(Vec::new())
                }),
            )
            .unwrap(),
            SignInStep::Server { problem: None }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::HaveCode {
                    url: "not a url".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Server {
                problem: Some(SignInProblem::UrlInvalid)
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::HaveCode { url: base.clone() }),
                adopt()
            )
            .unwrap(),
            SignInStep::Invite {
                server: base.clone(),
                problem: None
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Invite {
                    code: "   ".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Invite {
                server: base.clone(),
                problem: Some(SignInProblem::Other {
                    reason: "Paste the code you were given.".into()
                })
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Invite {
                    code: " join-code ".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Name {
                server: base.clone(),
                problem: None
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Name {
                    name: "n".repeat(81)
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Name {
                server: base.clone(),
                problem: Some(SignInProblem::Other {
                    reason: "Type the name people should see.".into()
                })
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Name {
                    name: " Ada ".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Email {
                server: base.clone(),
                problem: None
            }
        );
        assert_eq!(
            step(&hub, Some(SignInInput::Back), adopt()).unwrap(),
            SignInStep::Name {
                server: base.clone(),
                problem: None
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Name { name: "Ada".into() }),
                adopt()
            )
            .unwrap(),
            SignInStep::Email {
                server: base.clone(),
                problem: None
            }
        );
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Email {
                    email: "ada@example.com".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Password {
                hint: String::new(),
                problem: None
            }
        );
        assert_eq!(hits.lock().unwrap().len(), 0);
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Password {
                    password: "short".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Password {
                hint: String::new(),
                problem: Some(SignInProblem::Other {
                    reason: "Use 12 to 128 characters.".into()
                })
            }
        );
        assert_eq!(hits.lock().unwrap().len(), 0);
        assert_eq!(
            step(
                &hub,
                Some(SignInInput::Password {
                    password: "correct-horse".into()
                }),
                adopt()
            )
            .unwrap(),
            SignInStep::Done {
                account: Some(AccountKey {
                    network: Network::Tiny,
                    account: "22222222-2222-2222-2222-222222222222".into(),
                })
            }
        );
        let sent = hits.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].lines().next().unwrap_or("").starts_with("POST /v1/invites/accept "));
        let body: serde_json::Value = serde_json::from_str(sent[0].split("\r\n\r\n").nth(1).unwrap_or("")).unwrap();
        assert_eq!(body["token"], "join-code");
        assert_eq!(body["email"], "ada@example.com");
        assert_eq!(body["display_name"], "Ada");
        assert_eq!(body["password"], "correct-horse");
        let session = kept.lock().unwrap().clone().expect("adopted");
        let saved: serde_json::Value = serde_json::from_str(&session).unwrap();
        assert_eq!(saved["user_id"], "22222222-2222-2222-2222-222222222222");
        assert_eq!(saved["role"], "member");
        assert_eq!(saved["base_url"], base);
        assert_eq!(saved["display_name"], "Ada");
        cancel();
    }
}
