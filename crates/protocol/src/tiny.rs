//! A Tiny server's members, invites, and audit. No rooms and no messages.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TinyMember {
    pub user_id: String,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TinyInvite {
    pub id: String,
    pub role: String,
    pub created_by: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_by: Option<String>,
}

/// The token is only in the answer to creating an invite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TinyCode {
    pub id: String,
    pub token: String,
    pub role: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TinyAudit {
    pub id: i64,
    pub actor_id: Option<String>,
    pub action: String,
    pub subject: String,
    /// The server's detail object, as JSON text. Empty when the server sent none.
    pub detail: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TinyCommunity {
    pub role: String,
    pub members: Vec<TinyMember>,
    pub invites: Vec<TinyInvite>,
    pub audit: Vec<TinyAudit>,
}

/// Whether `role` may do `id`. An unknown id is refused.
pub fn tiny_can(role: &str, id: &str) -> bool {
    match id {
        "community.members.read" => matches!(role, "owner" | "admin" | "member"),
        "community.invites.read"
        | "community.invites.create"
        | "community.members.role"
        | "community.members.remove"
        | "community.audit.read" => matches!(role, "owner" | "admin"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_can_matrix() {
        assert!(tiny_can("member", "community.members.read"));
        assert!(tiny_can("admin", "community.members.read"));
        assert!(tiny_can("owner", "community.members.read"));
        assert!(!tiny_can("guest", "community.members.read"));
        for id in [
            "community.invites.read",
            "community.invites.create",
            "community.members.role",
            "community.members.remove",
            "community.audit.read",
        ] {
            assert!(!tiny_can("member", id), "{id}");
            assert!(tiny_can("admin", id), "{id}");
            assert!(tiny_can("owner", id), "{id}");
            assert!(!tiny_can("someone", id), "{id}");
        }
        assert!(!tiny_can("owner", "community.nope"));
        assert!(!tiny_can("admin", ""));
        assert!(!tiny_can("member", "community.invites.create"));
    }

    #[test]
    fn an_invite_omits_an_acceptance_it_does_not_have() {
        let invite = TinyInvite {
            id: "1".into(),
            role: "member".into(),
            created_by: "a".into(),
            expires_at: "t".into(),
            accepted_at: None,
            accepted_by: None,
        };
        let json = serde_json::to_string(&invite).unwrap();
        assert!(!json.contains("accepted"));
        let back: TinyInvite = serde_json::from_str(r#"{"id":"1","role":"member","created_by":"a","expires_at":"t"}"#).unwrap();
        assert_eq!(back, invite);
    }
}
