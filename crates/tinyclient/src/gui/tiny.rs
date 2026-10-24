//! The Tiny server window: who is on it, and an invite for the next person.
//! A control the account cannot use is absent, never disabled.

use super::components::{button, button_disabled, section_header, segment, segmented, ButtonKind};
use super::layout::{drags_window, TOP_BAR};
use super::sidebar::Mode;
use super::state::{Shell, Snapshot};
use super::theme::{self, Type};
use crate::client::ask;
use gpui::{div, prelude::*, px, AnyElement, ClipboardItem, Context, Div, Window};
use protocol::{AccountKey, Link, Network, Request, Response};
use std::io;

/// What the server window is holding. The invite token stays until Done and is never logged.
pub struct State {
    seen: bool,
    account: Option<AccountKey>,
    load_epoch: u64,
    action_epoch: u64,
    loading: bool,
    community: Option<protocol::TinyCommunity>,
    error: Option<String>,
    /// `"member"` or `"admin"`. Never owner.
    invite_role: String,
    token: Option<String>,
    busy: bool,
    confirm: Option<String>,
}

impl Default for State {
    fn default() -> State {
        State {
            seen: false,
            account: None,
            load_epoch: 0,
            action_epoch: 0,
            loading: false,
            community: None,
            error: None,
            invite_role: "member".into(),
            token: None,
            busy: false,
            confirm: None,
        }
    }
}

struct Member {
    user_id: String,
    name: String,
    email: String,
    role: String,
    make_admin: bool,
    make_member: bool,
    make_owner: bool,
    remove: bool,
}

struct OpenInvite {
    role: String,
    expires: String,
}

struct Ready {
    title: &'static str,
    line: Option<&'static str>,
    error: Option<String>,
    token: Option<String>,
    invite_role: String,
    can_invite: bool,
    busy: bool,
    confirm: Option<String>,
    members: Vec<Member>,
    invites: Vec<OpenInvite>,
    /// `None` when this account cannot read the audit.
    audit: Option<Vec<String>>,
}

enum Page {
    Loading,
    Failed(String),
    Ready(Ready),
}

fn connected(snapshot: &Snapshot) -> Option<AccountKey> {
    snapshot
        .accounts
        .iter()
        .find(|status| status.key.network == Network::Tiny && status.link == Link::Connected)
        .map(|status| status.key.clone())
}

fn same_role(role: &str, name: &str) -> bool {
    role.eq_ignore_ascii_case(name)
}

fn known_role(role: &str) -> bool {
    same_role(role, "owner") || same_role(role, "admin") || same_role(role, "member")
}

/// A role button is drawn only when that change would be legal.
fn role_change_legal(caller: &str, target: &str, next: &str, owners: usize) -> bool {
    if same_role(target, next) || !known_role(next) {
        return false;
    }
    // The only owner cannot be demoted.
    if same_role(target, "owner") && owners <= 1 {
        return false;
    }
    // Changing an owner, or granting owner, is the owner's act alone.
    if same_role(target, "owner") || same_role(next, "owner") {
        return same_role(caller, "owner");
    }
    if same_role(caller, "owner") {
        return true;
    }
    // An admin may promote a member, and does not demote another admin.
    same_role(caller, "admin") && same_role(target, "member") && same_role(next, "admin")
}

fn remove_legal(caller: &str, target: &str, owners: usize) -> bool {
    if same_role(target, "owner") && owners <= 1 {
        return false;
    }
    same_role(caller, "owner") || (same_role(caller, "admin") && same_role(target, "member"))
}

fn can_make(caller: &str, target: &str, next: &str, owners: usize) -> bool {
    protocol::tiny_can(caller, "community.members.role") && role_change_legal(caller, target, next, owners)
}

fn can_remove(caller: &str, target: &str, owners: usize) -> bool {
    protocol::tiny_can(caller, "community.members.remove") && remove_legal(caller, target, owners)
}

fn role_word(role: &str) -> String {
    if same_role(role, "owner") {
        "Owner".to_owned()
    } else if same_role(role, "admin") {
        "Admin".to_owned()
    } else if same_role(role, "member") {
        "Member".to_owned()
    } else {
        role.to_owned()
    }
}

fn person_name(display: &str, email: &str) -> (String, String) {
    if !display.is_empty() {
        (display.to_owned(), email.to_owned())
    } else if !email.is_empty() {
        (email.to_owned(), String::new())
    } else {
        ("Someone".to_owned(), String::new())
    }
}

impl Shell {
    /// Loads the server when the window is actually showing it, and again the next time it is.
    pub(super) fn note_tiny_page(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.tiny_shown() else {
            self.tiny_state.seen = false;
            return;
        };
        if self.tiny_state.seen && self.tiny_state.account.as_ref() == Some(&account) {
            return;
        }
        self.tiny_state.seen = true;
        self.fetch(account, None, cx);
    }

    pub(super) fn tiny_open(&self) -> bool {
        self.tiny_state.seen
    }

    fn tiny_shown(&self) -> Option<AccountKey> {
        if self.shown().empty_state().is_some() {
            return None;
        }
        if !matches!(self.sidebar_state.mode, Mode::Network(Network::Tiny)) {
            return None;
        }
        let snapshot = self.snapshot.as_ref()?;
        if snapshot.open.is_some() {
            return None;
        }
        connected(snapshot)
    }

    pub(super) fn tiny_page(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let page = self.page();
        let body = match &page {
            Page::Loading => note_line("Loading this server.", false).into_any_element(),
            Page::Failed(detail) => note_line(detail, true).into_any_element(),
            Page::Ready(ready) => ready_view(ready, cx),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                drags_window(div())
                    .h(px(TOP_BAR))
                    .flex_none()
                    .px(px(theme::PANE_PAD))
                    .flex()
                    .items_center()
                    .child(self.strip_controls(window, cx)),
            )
            .child(body)
    }

    fn page(&self) -> Page {
        let state = &self.tiny_state;
        if state.loading && state.community.is_none() {
            return Page::Loading;
        }
        let Some(community) = state.community.as_ref() else {
            return match state.error.clone() {
                Some(detail) => Page::Failed(detail),
                None => Page::Loading,
            };
        };
        let caller = community.role.as_str();
        let owners = community.members.iter().filter(|member| same_role(&member.role, "owner")).count();
        let can_invite = protocol::tiny_can(caller, "community.invites.create");
        let alone = community.members.len() == 1;
        let members = if protocol::tiny_can(caller, "community.members.read") {
            community
                .members
                .iter()
                .map(|member| {
                    let (name, email) = person_name(&member.display_name, &member.email);
                    let role = member.role.as_str();
                    Member {
                        user_id: member.user_id.clone(),
                        name,
                        email,
                        role: role_word(role),
                        make_admin: can_make(caller, role, "admin", owners),
                        make_member: can_make(caller, role, "member", owners),
                        make_owner: can_make(caller, role, "owner", owners),
                        remove: can_remove(caller, role, owners),
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        let invites = if protocol::tiny_can(caller, "community.invites.read") {
            community
                .invites
                .iter()
                .filter(|invite| invite.accepted_at.is_none())
                .map(|invite| OpenInvite { role: role_word(&invite.role), expires: invite.expires_at.to_string() })
                .collect()
        } else {
            Vec::new()
        };
        let audit = protocol::tiny_can(caller, "community.audit.read").then(|| {
            community.audit.iter().map(|row| format!("{} {}", row.action, row.subject)).collect()
        });
        Page::Ready(Ready {
            title: if alone && can_invite { "Just you so far" } else { "Tiny server" },
            line: (alone && can_invite && state.token.is_none())
                .then_some("Invite someone. They paste the code into their own tinyclient."),
            error: state.error.clone().filter(|detail| !detail.is_empty()),
            token: state.token.clone(),
            invite_role: state.invite_role.clone(),
            can_invite,
            busy: state.busy,
            confirm: state.confirm.clone(),
            members,
            invites,
            audit,
        })
    }

    /// `clear_busy` is the action whose button stays quiet until this read returns.
    fn fetch(&mut self, account: AccountKey, clear_busy: Option<u64>, cx: &mut Context<Self>) {
        let switched = self.tiny_state.account.as_ref() != Some(&account);
        if switched {
            self.tiny_state.community = None;
            self.tiny_state.token = None;
            self.tiny_state.confirm = None;
            self.tiny_state.error = None;
            self.tiny_state.invite_role = "member".into();
            self.tiny_state.action_epoch += 1;
            self.tiny_state.busy = false;
        }
        self.tiny_state.account = Some(account.clone());
        if self.tiny_state.community.is_none() {
            self.tiny_state.loading = true;
            self.tiny_state.error = None;
        }
        self.tiny_state.load_epoch += 1;
        let epoch = self.tiny_state.load_epoch;
        self.off_thread(cx, move || ask(&Request::TinyCommunity { account }), move |this, answer, cx| {
            if this.tiny_state.load_epoch != epoch {
                return;
            }
            this.tiny_state.loading = false;
            if let Some(action) = clear_busy {
                if this.tiny_state.action_epoch == action {
                    this.tiny_state.busy = false;
                }
            }
            match answer {
                Ok(Response::TinyCommunity { community }) => {
                    let confirm = this.tiny_state.confirm.clone();
                    if confirm.as_ref().is_some_and(|id| community.members.iter().all(|member| &member.user_id != id)) {
                        this.tiny_state.confirm = None;
                    }
                    this.tiny_state.error = None;
                    this.tiny_state.community = Some(community);
                }
                Ok(Response::Error { detail, .. }) => this.tiny_state.error = Some(detail),
                Ok(_) => this.tiny_state.error = Some(crate::client::OTHER_ANSWER.to_owned()),
                Err(err) => this.tiny_state.error = Some(crate::client::trouble_words(&err)),
            }
            cx.notify();
        });
    }

    fn act(&mut self, request: Request, cx: &mut Context<Self>) {
        if self.tiny_state.busy {
            return;
        }
        let Some(account) = self.tiny_state.account.clone() else {
            return;
        };
        self.tiny_state.busy = true;
        self.tiny_state.error = None;
        self.tiny_state.action_epoch += 1;
        let epoch = self.tiny_state.action_epoch;
        cx.notify();
        self.off_thread(cx, move || ask(&request), move |this, answer, cx| this.acted(epoch, account, answer, cx));
    }

    fn acted(&mut self, epoch: u64, account: AccountKey, answer: io::Result<Response>, cx: &mut Context<Self>) {
        if self.tiny_state.action_epoch != epoch {
            return;
        }
        if self.tiny_state.account.as_ref() != Some(&account) {
            self.tiny_state.busy = false;
            cx.notify();
            return;
        }
        match answer {
            Ok(Response::Ok) => {
                self.tiny_state.confirm = None;
                self.tiny_state.error = None;
                self.fetch(account, Some(epoch), cx);
                cx.notify();
            }
            Ok(Response::TinyCode { code }) => {
                self.tiny_state.token = Some(code.token);
                self.tiny_state.error = None;
                self.fetch(account, Some(epoch), cx);
                cx.notify();
            }
            Ok(Response::Error { detail, .. }) => {
                self.tiny_state.busy = false;
                self.tiny_state.error = Some(detail);
                cx.notify();
            }
            Ok(_) => {
                self.tiny_state.busy = false;
                self.tiny_state.error = Some(crate::client::OTHER_ANSWER.to_owned());
                cx.notify();
            }
            Err(err) => {
                self.tiny_state.busy = false;
                self.tiny_state.error = Some(crate::client::trouble_words(&err));
                cx.notify();
            }
        }
    }

    fn send_invite(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.tiny_state.account.clone() else {
            return;
        };
        let role = if self.tiny_state.invite_role == "admin" { "admin" } else { "member" };
        self.act(Request::TinyInvite { account, role: role.to_owned() }, cx);
    }

    fn pick_role(&mut self, role: &'static str, cx: &mut Context<Self>) {
        if self.tiny_state.busy || (role != "member" && role != "admin") {
            return;
        }
        self.tiny_state.invite_role = role.to_owned();
        cx.notify();
    }

    fn send_role(&mut self, user_id: String, role: &'static str, cx: &mut Context<Self>) {
        let Some(account) = self.tiny_state.account.clone() else {
            return;
        };
        self.act(Request::TinyRole { account, user_id, role: role.to_owned() }, cx);
    }

    fn ask_remove(&mut self, user_id: String, cx: &mut Context<Self>) {
        if self.tiny_state.busy {
            return;
        }
        self.tiny_state.confirm = Some(user_id);
        cx.notify();
    }

    fn cancel_remove(&mut self, cx: &mut Context<Self>) {
        self.tiny_state.confirm = None;
        cx.notify();
    }

    fn send_remove(&mut self, user_id: String, cx: &mut Context<Self>) {
        let Some(account) = self.tiny_state.account.clone() else {
            return;
        };
        self.tiny_state.confirm = None;
        self.act(Request::TinyRemove { account, user_id }, cx);
    }

    fn copy_code(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.tiny_state.token.clone() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(token));
    }

    fn done_code(&mut self, cx: &mut Context<Self>) {
        self.tiny_state.token = None;
        cx.notify();
    }
}

fn note_line(words: &str, bad: bool) -> Div {
    let line = div().flex_1().px(px(theme::PANE_PAD)).pt(px(8.));
    if bad {
        line.type_small().text_color(theme::danger()).child(words.to_owned())
    } else {
        line.type_ui().text_color(theme::text_soft()).child(words.to_owned())
    }
}

fn ready_view(ready: &Ready, cx: &mut Context<Shell>) -> AnyElement {
    let mut column = div().w_full().pt(px(8.)).pb(px(24.)).flex().flex_col().child(
        div()
            .px(px(theme::PANE_PAD))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().type_title().text_color(theme::text()).child(ready.title))
            .children(ready.line.map(|line| div().max_w(px(360.)).whitespace_normal().type_ui().text_color(theme::text_soft()).child(line)))
            .children(ready.error.as_ref().map(|detail| div().type_small().text_color(theme::danger()).child(detail.clone())))
            .children(ready.token.as_ref().map(|token| code_panel(token, cx)))
            .when(ready.token.is_none() && ready.can_invite, |column| column.child(invite_row(&ready.invite_role, ready.busy, cx))),
    );
    if !ready.members.is_empty() {
        column = column.child(div().mt(px(8.)).flex().flex_col().children(ready.members.iter().map(|member| member_row(member, ready.confirm.as_deref(), cx))));
    }
    if !ready.invites.is_empty() {
        column = column.child(invite_section(&ready.invites));
    }
    if let Some(rows) = &ready.audit {
        column = column.child(audit_section(rows));
    }
    div().id("tiny-page").flex_1().min_h_0().overflow_y_scroll().child(column).into_any_element()
}

fn invite_row(role: &str, busy: bool, cx: &mut Context<Shell>) -> Div {
    let invite = if busy {
        button_disabled("Invite someone").into_any_element()
    } else {
        button("tiny-invite", ButtonKind::Primary, "Invite someone")
            .on_click(cx.listener(|this, _, _, cx| this.send_invite(cx)))
            .into_any_element()
    };
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(8.))
        .child(
            segmented()
                .child(segment("tiny-as-member", "Member", role != "admin").on_click(cx.listener(|this, _, _, cx| this.pick_role("member", cx))))
                .child(segment("tiny-as-admin", "Admin", role == "admin").on_click(cx.listener(|this, _, _, cx| this.pick_role("admin", cx)))),
        )
        .child(invite)
}

fn code_panel(token: &str, cx: &mut Context<Shell>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(div().whitespace_normal().type_ui().text_color(theme::text_soft()).child("Copy this code. It works for 7 days and will not be shown again."))
        .child(
            div()
                .px(px(12.))
                .py(px(8.))
                .rounded(px(theme::R_CONTROL))
                .bg(theme::field())
                .border_1()
                .border_color(theme::border())
                .whitespace_normal()
                .type_code()
                .child(token.to_owned()),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.))
                .child(button("tiny-copy", ButtonKind::Primary, "Copy code").on_click(cx.listener(|this, _, _, cx| this.copy_code(cx))))
                .child(button("tiny-done", ButtonKind::Secondary, "Done").on_click(cx.listener(|this, _, _, cx| this.done_code(cx)))),
        )
}

fn member_row(member: &Member, confirm: Option<&str>, cx: &mut Context<Shell>) -> Div {
    let confirming = confirm == Some(member.user_id.as_str());
    let identity = div().flex_1().min_w(px(120.)).flex().flex_col().child(
        div()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(div().min_w_0().truncate().type_ui().text_color(theme::text()).child(member.name.clone()))
            .child(div().flex_none().type_small().text_color(theme::text_soft()).child(member.role.clone())),
    )
    .when(!member.email.is_empty(), |identity| {
        identity.child(div().min_w_0().truncate().type_small().text_color(theme::text_faint()).child(member.email.clone()))
    });
    div().w_full().px(px(theme::PANE_PAD)).py(px(6.)).flex().flex_col().gap(px(6.)).child(
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .child(identity)
            .when(!confirming, |row| row.child(member_actions(member, cx)))
            .when(confirming, |row| row.child(confirm_remove(&member.name, &member.user_id, cx))),
    )
}

fn member_actions(member: &Member, cx: &mut Context<Shell>) -> Div {
    let mut actions = div().flex_none().flex().flex_wrap().items_center().gap(px(4.));
    if member.make_admin {
        let user_id = member.user_id.clone();
        actions = actions.child(
            button(format!("tiny-admin-{}", member.user_id), ButtonKind::Secondary, "Make admin")
                .on_click(cx.listener(move |this, _, _, cx| this.send_role(user_id.clone(), "admin", cx))),
        );
    }
    if member.make_member {
        let user_id = member.user_id.clone();
        actions = actions.child(
            button(format!("tiny-member-{}", member.user_id), ButtonKind::Secondary, "Make member")
                .on_click(cx.listener(move |this, _, _, cx| this.send_role(user_id.clone(), "member", cx))),
        );
    }
    if member.make_owner {
        let user_id = member.user_id.clone();
        actions = actions.child(
            button(format!("tiny-owner-{}", member.user_id), ButtonKind::Secondary, "Make owner")
                .on_click(cx.listener(move |this, _, _, cx| this.send_role(user_id.clone(), "owner", cx))),
        );
    }
    if member.remove {
        let user_id = member.user_id.clone();
        actions = actions.child(
            button(format!("tiny-remove-{}", member.user_id), ButtonKind::Danger, "Remove")
                .on_click(cx.listener(move |this, _, _, cx| this.ask_remove(user_id.clone(), cx))),
        );
    }
    actions
}

fn confirm_remove(name: &str, user_id: &str, cx: &mut Context<Shell>) -> Div {
    let remove_id = user_id.to_owned();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(8.))
        .child(div().whitespace_normal().type_ui().text_color(theme::text()).child(format!("Remove {name} from this server?")))
        .child(
            button(format!("tiny-remove-yes-{user_id}"), ButtonKind::Danger, "Remove")
                .on_click(cx.listener(move |this, _, _, cx| this.send_remove(remove_id.clone(), cx))),
        )
        .child(button(format!("tiny-remove-no-{user_id}"), ButtonKind::Secondary, "Cancel").on_click(cx.listener(|this, _, _, cx| this.cancel_remove(cx))))
}

fn invite_section(invites: &[OpenInvite]) -> Div {
    div().mt(px(12.)).flex().flex_col().child(section_header("Invites", true)).children(invites.iter().map(|invite| {
        div()
            .px(px(theme::PANE_PAD))
            .h(px(theme::ROW_ONE))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().type_ui().text_color(theme::text()).child(invite.role.clone()))
            .child(div().type_small().text_color(theme::text_soft()).child(format!("Expires {}", invite.expires)))
    }))
}

fn audit_section(rows: &[String]) -> Div {
    let mut section = div().mt(px(12.)).flex().flex_col().child(section_header("Activity", true));
    if rows.is_empty() {
        section = section.child(div().px(px(theme::PANE_PAD)).type_ui().text_color(theme::text_soft()).child("Nothing has been recorded."));
    } else {
        section = section.children(rows.iter().map(|row| {
            div().px(px(theme::PANE_PAD)).h(px(theme::ROW_ONE)).flex().items_center().min_w_0().truncate().type_ui().text_color(theme::text()).child(row.clone())
        }));
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_buttons_follow_who_may_change_whom() {
        let owners = 1;
        assert!(role_change_legal("owner", "member", "admin", owners));
        assert!(role_change_legal("owner", "member", "owner", owners));
        assert!(role_change_legal("admin", "member", "admin", owners));
        assert!(!role_change_legal("admin", "member", "owner", owners));
        assert!(!role_change_legal("admin", "admin", "member", owners));
        assert!(!role_change_legal("admin", "owner", "admin", owners));
        assert!(!role_change_legal("admin", "owner", "member", owners));
        assert!(!role_change_legal("owner", "owner", "admin", owners));
        assert!(!role_change_legal("owner", "owner", "member", owners));
        assert!(!role_change_legal("member", "member", "admin", owners));
        assert!(role_change_legal("owner", "owner", "admin", 2));
        assert!(role_change_legal("owner", "admin", "member", owners));
        assert!(!role_change_legal("owner", "admin", "admin", owners));
    }

    #[test]
    fn remove_spares_the_only_owner_and_an_admin_does_not_touch_one() {
        assert!(!remove_legal("owner", "owner", 1));
        assert!(remove_legal("owner", "owner", 2));
        assert!(remove_legal("owner", "admin", 1));
        assert!(remove_legal("owner", "member", 1));
        assert!(remove_legal("admin", "member", 1));
        assert!(!remove_legal("admin", "admin", 1));
        assert!(!remove_legal("admin", "owner", 2));
        assert!(!remove_legal("member", "member", 1));
    }
}
