//! A sign-in tinyclient draws itself, one step at a time, for a network that has no web page to
//! show (Telegram). The shell asks chatd to begin, draws the step it is told, and answers it;
//! chatd tells the step again whenever it moves on its own (`Change::SignIn`).
//!
//! What the user types (a number, a code, a password) crosses the local socket once, inside
//! [`SignInInput`], and is never stored or logged by either side.

use crate::AccountKey;
use serde::{Deserialize, Serialize};
use std::fmt;

/// How a login code reached the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeVia {
    /// A message in the network's own app on another device.
    App,
    Sms,
    Call,
    Email,
    Other,
}

/// Why the step is being shown again. The shell has the words for each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "problem", rename_all = "snake_case")]
pub enum SignInProblem {
    /// The network could not be reached.
    Unreachable,
    PhoneInvalid,
    /// No account uses this number, and tinyclient does not create accounts.
    PhoneUnknown,
    PhoneBanned,
    CodeInvalid,
    CodeExpired,
    PasswordInvalid,
    /// Not an http or https host. A user in the address is refused. A path, query, or fragment is dropped.
    UrlInvalid,
    /// Not one address: missing `@`, more than one, or whitespace.
    EmailInvalid,
    /// The network will not take another try before `until` (unix seconds).
    TooMany { until: i64 },
    /// Anything else, in words the user can read.
    Other { reason: String },
}

/// Where a sign-in stands: what the shell draws now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum SignInStep {
    /// Scan `link` as a QR code with the network's phone app. Empty until the network has issued
    /// one. The network replaces it at `expires` (unix seconds); the new one is a `Change`.
    Qr {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        link: String,
        #[serde(default)]
        expires: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<SignInProblem>,
    },
    /// Ask for the phone number, in international form.
    Phone {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<SignInProblem>,
    },
    /// A code of `length` digits was sent `via`, for the number `phone` as the user typed it.
    Code {
        via: CodeVia,
        length: u8,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        phone: String,
        /// How `Resend` would send it, when the network offers another way.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<CodeVia>,
        /// Unix seconds before which `Resend` is refused. 0: any time.
        #[serde(default)]
        resend_at: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<SignInProblem>,
    },
    /// The account has a second password. `hint` is the user's own, as the network keeps it.
    /// A Tiny server uses this step for the account password; `hint` is empty there.
    Password {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        hint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<SignInProblem>,
    },
    /// Ask for a Tiny server's address. `problem` is skipped when there is none.
    Server {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<SignInProblem>,
    },
    /// Ask for the email on that server. `server` is the address that was accepted.
    Email {
        server: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<SignInProblem>,
    },
    /// Ask for an invite code for `server`.
    Invite {
        server: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<SignInProblem>,
    },
    /// Ask for the name people should see.
    Name {
        server: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<SignInProblem>,
    },
    /// Signed in. The account is in `Status` from now on.
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<AccountKey>,
    },
    /// It cannot go on. `reason` is in words; beginning again starts over.
    Failed { reason: String },
}

/// What the user did about a step.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum SignInInput {
    /// Scan a code instead of typing a number.
    UseQr,
    /// Type a number instead of scanning.
    UsePhone,
    Phone { number: String },
    Code { code: String },
    Password { password: String },
    /// A Tiny server's address, as typed.
    Server { url: String },
    /// The email on that server, as typed.
    Email { email: String },
    /// A Tiny server's address, to join it with a code.
    HaveCode { url: String },
    /// The invite code, as pasted.
    Invite { code: String },
    /// The name people should see.
    Name { name: String },
    /// Send the code again, the way `SignInStep::Code::next` says.
    Resend,
    /// One step back: from the code to the number, from the number to the QR code.
    Back,
}

/// Never prints what was typed: a request may be logged whole.
impl fmt::Debug for SignInInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SignInInput::UseQr => "UseQr",
            SignInInput::UsePhone => "UsePhone",
            SignInInput::Phone { .. } => "Phone { .. }",
            SignInInput::Code { .. } => "Code { .. }",
            SignInInput::Password { .. } => "Password { .. }",
            SignInInput::Server { .. } => "Server { .. }",
            SignInInput::Email { .. } => "Email { .. }",
            SignInInput::HaveCode { .. } => "HaveCode { .. }",
            SignInInput::Invite { .. } => "Invite { .. }",
            SignInInput::Name { .. } => "Name { .. }",
            SignInInput::Resend => "Resend",
            SignInInput::Back => "Back",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_was_typed_is_never_printed_and_bare_steps_read() {
        let typed = SignInInput::Password { password: "hunter2".into() };
        assert!(!format!("{typed:?}").contains("hunter2"));
        assert!(!format!("{:?}", SignInInput::Code { code: "12345".into() }).contains("12345"));
        let url = SignInInput::Server { url: "http://127.0.0.1:8080".into() };
        let email = SignInInput::Email { email: "ada@example.com".into() };
        assert_eq!(format!("{url:?}"), "Server { .. }");
        assert_eq!(format!("{email:?}"), "Email { .. }");
        assert!(!format!("{url:?}").contains("127.0.0.1"));
        assert!(!format!("{email:?}").contains("ada@"));
        assert!(!format!("{email:?}").contains("example.com"));
        let invite = SignInInput::Invite { code: "secret-code".into() };
        let name = SignInInput::Name { name: "Ada Lovelace".into() };
        assert_eq!(format!("{invite:?}"), "Invite { .. }");
        assert_eq!(format!("{name:?}"), "Name { .. }");
        assert!(!format!("{invite:?}").contains("secret-code"));
        assert!(!format!("{name:?}").contains("Ada"));
        assert!(!format!("{name:?}").contains("Lovelace"));
        // A step with nothing but its name still reads: every other field is optional.
        assert_eq!(serde_json::from_str::<SignInStep>(r#"{"step":"server"}"#).unwrap(), SignInStep::Server { problem: None });
        let email_step = SignInStep::Email { server: "http://127.0.0.1:8080".into(), problem: None };
        assert_eq!(serde_json::to_string(&email_step).unwrap(), r#"{"step":"email","server":"http://127.0.0.1:8080"}"#);
        assert_eq!(serde_json::from_str::<SignInStep>(r#"{"step":"phone"}"#).unwrap(), SignInStep::Phone { problem: None });
        assert_eq!(
            serde_json::from_str::<SignInStep>(r#"{"step":"qr"}"#).unwrap(),
            SignInStep::Qr { link: String::new(), expires: 0, problem: None }
        );
        let code = SignInStep::Code { via: CodeVia::App, length: 5, phone: String::new(), next: None, resend_at: 0, problem: None };
        assert_eq!(serde_json::to_string(&code).unwrap(), r#"{"step":"code","via":"app","length":5,"resend_at":0}"#);
    }
}
