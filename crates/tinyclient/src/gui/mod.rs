//! The window. It holds what is on screen and nothing else; chatd owns the rest.
//!
//! One file per region, so they can change without meeting: `state` (what the shell knows and
//! how it stays current), `layout`, `rail`, `sidebar`, `conversation`, `people`, `composer`,
//! `call`. Each view keeps its own `State` in its own file; `state::Shell` only holds them.

mod anim;
mod app;
mod assets;
mod call;
mod components;
mod composer;
mod conversation;
mod input;
mod layout;
mod pane;
mod people;
mod playing;
mod rail;
mod settings;
mod sidebar;
mod signin;
mod shortcuts;
mod state;
mod theme;
mod tiny;
mod time;

use gpui::{point, prelude::*, px, size, App, Bounds, Context, Div, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions};
use state::Shell;

/// The windows' Wayland app id and X11 class: a desktop matches them to `dev.tinyclient.app.desktop`
/// (packaging/linux) for the dock icon and name. The same id as the macOS bundle's.
pub(crate) const APP_ID: &str = "dev.tinyclient.app";

gpui::actions!(
    tinyclient,
    [
        Quit, Switcher, Back, Forward, UpLevel, ToggleSidebar, CloseRight, CloseTop, NextRow,
        PreviousRow, NextUnread, PreviousUnread, ListUp, ListDown, ListOpen, ListFold, ListUnfold,
        MarkRead, FocusNext, FocusPrevious,
        Place1, Place2, Place3, Place4, Place5, Place6, Place7, Place8, Place9,
        // The menu bar's (`app::menubar`).
        About, CheckForUpdates, Settings, Hide, HideOthers, ShowAll, NewMessage, AddAccount,
        AttachFile, CloseWindow, SearchConversation, SearchEverywhere, BiggerText, SmallerText,
        ActualSize, FullScreen, MarkUnread, MarkSpaceRead, ShowDetails, Star, Mute, StartCall,
        OpenInNetwork, CopyLink, Minimize, Zoom, ShowWindow, KeyboardShortcuts, ReleaseNotes,
        CopyDiagnostics, ReportProblem
    ]
);

impl Shell {
    /// `cmd-1` to `cmd-9`: the rail's places in order.
    fn visit_place(&mut self, index: usize, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if let Some(mode) = self.places().get(index).cloned() {
            self.visit(mode, window, cx);
        }
    }

    /// The shell itself has the keys: no field, no message, no menu.
    fn in_lists(&self, window: &gpui::Window) -> bool {
        self.focus.is_focused(window) || self.sidebar_state.focus.is_focused(window)
    }

    /// A character typed with no field focused goes to the composer.
    fn typed(&mut self, event: &gpui::KeyDownEvent, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let keys = &event.keystroke;
        let command = keys.modifiers.platform || keys.modifiers.control || keys.modifiers.function;
        let Some(text) = keys.key_char.as_deref().filter(|text| !command && typed_text(text)) else {
            return;
        };
        let layered = self.sidebar_state.switcher.is_some() || self.layout_state.menu.is_some();
        if layered || self.open.is_none() || !self.in_lists(window) {
            return;
        }
        self.composer_type(text, window, cx);
        cx.stop_propagation();
    }

    /// `tab` and `shift-tab`: the keys move between the sidebar's list and the composer. The
    /// region that has them shows it on its row or its field, never around a pane.
    // The messages and the right pane are the conversation view's; they take the keys by click.
    fn focus_region(&mut self, forward: bool, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let _ = forward;
        if self.in_lists(window) && self.open.is_some() {
            self.composer_focus(window, cx);
            return;
        }
        window.focus(&self.sidebar_state.focus, cx);
        self.cursor_to_open();
        cx.notify();
    }

    /// The window's keys, on its root so they work wherever focus is. A text field's own keys
    /// win while it has focus.
    fn keys(&self, root: Div, cx: &mut Context<Self>) -> Div {
        macro_rules! places {
            ($root:expr, $($action:ty => $index:expr),*) => {
                $root$(.on_action(cx.listener(|this, _: &$action, window, cx| this.visit_place($index, window, cx))))*
            };
        }
        let root = places!(root, Place1 => 0, Place2 => 1, Place3 => 2, Place4 => 3, Place5 => 4, Place6 => 5, Place7 => 6, Place8 => 7, Place9 => 8);
        root.track_focus(&self.focus)
            .key_context("Shell")
            .on_action(cx.listener(|this, _: &Switcher, window, cx| this.open_switcher("", window, cx)))
            .on_action(cx.listener(|this, _: &Back, _, cx| this.travel(true, cx)))
            .on_action(cx.listener(|this, _: &Forward, _, cx| this.travel(false, cx)))
            .on_action(cx.listener(|this, _: &UpLevel, _, cx| this.up_level(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)))
            .on_action(cx.listener(|this, _: &MarkRead, _, cx| this.mark_open_read(cx)))
            .on_action(cx.listener(|this, _: &FocusNext, window, cx| this.focus_region(true, window, cx)))
            .on_action(cx.listener(|this, _: &FocusPrevious, window, cx| this.focus_region(false, window, cx)))
            .on_key_down(cx.listener(Self::typed))
            .on_action(cx.listener(|this, _: &CloseRight, window, cx| this.close_right(Some(window), cx)))
            .on_action(cx.listener(|this, _: &SearchConversation, window, cx| this.search_here(window, cx)))
            .on_action(cx.listener(|this, _: &SearchEverywhere, window, cx| this.search_everywhere(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTop, window, cx| {
                // With no layer to close, Escape belongs to the conversation.
                if !this.close_top(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &NextRow, _, cx| this.walk(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousRow, _, cx| this.walk(false, cx)))
            .on_action(cx.listener(|this, _: &NextUnread, _, cx| this.walk_unread(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousUnread, _, cx| this.walk_unread(false, cx)))
            .on_action(cx.listener(|this, _: &ListDown, _, cx| {
                if !this.first_run_key("down", cx) {
                    this.move_cursor(true, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ListUp, _, cx| {
                if !this.first_run_key("up", cx) {
                    this.move_cursor(false, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ListOpen, _, cx| {
                if !this.first_run_key("enter", cx) {
                    this.open_cursor(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ListFold, _, cx| this.fold_cursor(false, cx)))
            .on_action(cx.listener(|this, _: &ListUnfold, _, cx| this.fold_cursor(true, cx)))
            .on_action(cx.listener(|this, _: &NewMessage, window, cx| this.open_switcher("@", window, cx)))
            .on_action(cx.listener(|this, _: &AddAccount, _, cx| this.open_menu(layout::Menu::Add, cx)))
            .on_action(cx.listener(|this, _: &BiggerText, window, cx| this.step_text(Some(1), window, cx)))
            .on_action(cx.listener(|this, _: &SmallerText, window, cx| this.step_text(Some(-1), window, cx)))
            .on_action(cx.listener(|this, _: &ActualSize, window, cx| this.step_text(None, window, cx)))
            .on_action(cx.listener(|_, _: &FullScreen, window, _| window.toggle_fullscreen()))
            .on_action(cx.listener(|_, _: &Minimize, window, _| window.minimize_window()))
            .on_action(cx.listener(|_, _: &Zoom, window, _| window.zoom_window()))
            .on_action(cx.listener(|this, _: &CopyDiagnostics, _, cx| this.copy_diagnostics(cx)))
            .when_some(self.snapshot.as_ref().and_then(|s| s.open.as_ref()), |root, open| {
                let (key, starred) = (open.key.clone(), open.starred);
                root.on_action(cx.listener(move |this, _: &Star, _, cx| this.star(key.clone(), !starred, cx)))
            })
    }
}

/// Whether a key's text is something to type: printable, and not the keys the lists use.
fn typed_text(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| !c.is_control()) && text != " "
}

fn bind_keys(cx: &mut App) {
    app::menubar::bind_keys(cx);
    cx.bind_keys(shell_bindings());
}

fn shell_bindings() -> Vec<KeyBinding> {
    let shell = Some("Shell");
    vec![
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-k", Switcher, None),
        KeyBinding::new("secondary-[", Back, None),
        KeyBinding::new("secondary-]", Forward, None),
        KeyBinding::new("secondary-up", UpLevel, shell),
        KeyBinding::new("secondary-\\", ToggleSidebar, None),
        KeyBinding::new("secondary-.", CloseRight, None),
        KeyBinding::new("escape", CloseTop, shell),
        KeyBinding::new("shift-escape", MarkRead, shell),
        KeyBinding::new("tab", FocusNext, shell),
        KeyBinding::new("shift-tab", FocusPrevious, shell),
        KeyBinding::new("alt-down", NextRow, None),
        KeyBinding::new("alt-up", PreviousRow, None),
        KeyBinding::new("alt-shift-down", NextUnread, None),
        KeyBinding::new("alt-shift-up", PreviousUnread, None),
        KeyBinding::new("down", ListDown, shell),
        KeyBinding::new("up", ListUp, shell),
        KeyBinding::new("enter", ListOpen, shell),
        KeyBinding::new("left", ListFold, shell),
        KeyBinding::new("right", ListUnfold, shell),
        KeyBinding::new("secondary-1", Place1, None),
        KeyBinding::new("secondary-2", Place2, None),
        KeyBinding::new("secondary-3", Place3, None),
        KeyBinding::new("secondary-4", Place4, None),
        KeyBinding::new("secondary-5", Place5, None),
        KeyBinding::new("secondary-6", Place6, None),
        KeyBinding::new("secondary-7", Place7, None),
        KeyBinding::new("secondary-8", Place8, None),
        KeyBinding::new("secondary-9", Place9, None),
    ]
}

/// `--shot out.png` draws one state with live data, writes the frame and quits, so every state
/// can be looked at without clicking:
/// `--size WxH` / `--narrow`, `--mode inbox|people|discord|slack|space` (`--space N` picks
/// which), `--filter unread|mentions`, `--open-first` / `--open N` (a row of the list on show),
/// `--cursor N`, `--switcher [letters]`, `--menu add|own|network|space|row`, `--float`,
/// `--hide-sidebar`, `--right thread|members`, `--pin N`, `--toast`, `--components`,
/// `--walk N` (N presses of the unread walk), `--back N` (N steps back in history),
/// `--sign-in-failed closed|unreachable|blocked` (first launch, with nothing signed in),
/// `--state starting|keychain|denied|reconnecting|failed` (a window state, forced),
/// `--crash-toast` (the toast after a crash relaunch), `--menu row-right|row-bottom` (the row
/// menu opened at the window's edges), `--text-step N`, `--start-chatd` (let the shot start
/// chatd when it is not running; off by default so a shot leaves no process behind),
/// `--autoplay always|hover|never` (when GIFs play), `--frames N` (N frames 150 ms apart, as
/// `out-1.png` …, to see moving pictures move).
#[cfg(feature = "gui-shot")]
mod shot {
    use super::layout::{Menu, RightKind, RightPane};
    use super::sidebar::Mode;
    use super::state::Shell;
    use super::app::service::Shown;
    use super::{components, flag, theme};
    use gpui::{div, point, prelude::*, px, App, Context, Window};
    use protocol::{InboxFilter, Network};
    use std::time::Duration;

    fn has(name: &str) -> bool {
        std::env::args().any(|arg| arg == name)
    }

    fn demo_pane(_: &Shell, _: &mut Window, _: &mut Context<Shell>) -> gpui::AnyElement {
        components::empty_state("icons/message-square.svg", "No replies yet", "This pane is filled by another view.").into_any_element()
    }

    fn stage(shell: &mut Shell, step: usize, window: &mut Window, cx: &mut Context<Shell>) {
        match step {
            0 => {
                if let Some(autoplay) = flag("--autoplay").and_then(|word| super::anim::Autoplay::from_word(&word)) {
                    super::anim::set_autoplay(autoplay);
                }
                if let Some(how) = flag("--sign-in-failed") {
                    use protocol::ErrorCode;
                    let (code, detail) = match how.as_str() {
                        "unreachable" => (ErrorCode::SignInUnreachable, ""),
                        "blocked" => (ErrorCode::SignInBlocked, ""),
                        _ => (ErrorCode::SignIn, "The sign-in window stopped before it finished."),
                    };
                    shell.sign_in_failed = super::state::sign_in_words(Network::Slack, code, detail).map(|words| (Network::Slack, words));
                }
                if let Some(index) = flag("--pin").and_then(|n| n.parse::<usize>().ok()) {
                    if let Some(space) = shell.spaces.get(index) {
                        shell.pin(space.key.clone(), true, cx);
                    }
                }
                let space = flag("--space").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
                match flag("--mode").as_deref() {
                    Some("people") => shell.go(Mode::People, cx),
                    Some("discord") => shell.go(Mode::Network(Network::Discord), cx),
                    Some("slack") => shell.go(Mode::Network(Network::Slack), cx),
                    Some("space") => {
                        if let Some(space) = shell.spaces.get(space) {
                            shell.go(Mode::Space(space.key.clone()), cx);
                        }
                    }
                    _ => {}
                }
                match flag("--filter").as_deref() {
                    Some("unread") => shell.set_filter(InboxFilter::Unread, cx),
                    Some("mentions") => shell.set_filter(InboxFilter::Mentions, cx),
                    _ => {}
                }
            }
            1 => {
                let row = if has("--open-first") { Some(0) } else { flag("--open").and_then(|n| n.parse::<usize>().ok()) };
                if let Some(row) = row {
                    shell.open_listed(row, cx);
                }
            }
            _ => {
                if let Some(cursor) = flag("--cursor").and_then(|n| n.parse::<usize>().ok()) {
                    shell.sidebar_state.cursor = Some(cursor);
                }
                if has("--float") {
                    shell.layout_state.floating = true;
                }
                if has("--hide-sidebar") {
                    shell.layout_state.sidebar_hidden = true;
                }
                if has("--focus-list") {
                    window.focus(&shell.sidebar_state.focus, cx);
                    shell.cursor_to_open();
                }
                match flag("--right").as_deref() {
                    Some("info") => shell.toggle_info(cx),
                    Some("members") => shell.open_right(RightPane { kind: RightKind::Members, title: "Members".into(), count: Some(128), body: demo_pane }, cx),
                    Some(_) => shell.open_right(RightPane { kind: RightKind::Thread, title: "Thread".into(), count: Some(6), body: demo_pane }, cx),
                    None => {}
                }
                match flag("--menu").as_deref() {
                    Some("add") => shell.open_menu(Menu::Add, cx),
                    Some("own") => shell.open_menu(Menu::Own, cx),
                    Some("network") => shell.open_menu(Menu::Network(shell.sidebar_state.mode.network().unwrap_or(Network::Discord)), cx),
                    Some("space") => {
                        if let Mode::Space(space) = shell.sidebar_state.mode.clone() {
                            shell.open_menu(Menu::Space(space), cx);
                        }
                    }
                    Some("row") => {
                        if let Some(row) = shell.sidebar_state.inbox.get(1).cloned() {
                            shell.open_menu(Menu::Row(row, point(px(240.), px(180.))), cx);
                        }
                    }
                    _ => {}
                }
                match flag("--state").as_deref() {
                    Some("starting") => shell.forced_state = Some(Shown::Starting),
                    Some("keychain") => shell.forced_state = Some(Shown::KeychainWaiting),
                    Some("denied") => shell.forced_state = Some(Shown::KeychainDenied(vec![Network::Slack])),
                    Some("reconnecting") => shell.forced_state = Some(Shown::Reconnecting),
                    Some("failed") => shell.forced_state = Some(Shown::Failed),
                    _ => {}
                }
                if has("--crash-toast") {
                    shell.crashed = Some(0);
                }
                let size = window.viewport_size();
                let (wide, tall) = (f32::from(size.width), f32::from(size.height));
                let edge = match flag("--menu").as_deref() {
                    Some("row-right") => Some(point(px(wide - 40.), px(200.))),
                    Some("row-bottom") => Some(point(px(wide - 40.), px(tall - 30.))),
                    _ => None,
                };
                if let (Some(at), Some(row)) = (edge, shell.sidebar_state.inbox.get(1).cloned()) {
                    shell.open_menu(Menu::Row(row, at), cx);
                }
                if let Some(step) = flag("--text-step").and_then(|n| n.parse::<i32>().ok()) {
                    theme::set_text_step(step);
                }
                if has("--toast") {
                    shell.toast(Some(("icons/check.svg", theme::text_soft())), "Marked 14 conversations read", cx);
                }
                if has("--switcher") {
                    let letters = flag("--switcher").filter(|next| !next.starts_with("--")).unwrap_or_default();
                    shell.open_switcher(&letters, window, cx);
                }
                if has("--components") {
                    shell.sheet = true;
                }
                cx.notify();
            }
        }
    }

    /// The base components in their states, for looking at next to `mockups/01-system.html`.
    /// Drawn over the whole window.
    pub fn sheet() -> gpui::Div {
        use components::*;
        use theme::Type;
        use protocol::Presence;
        let line = || div().flex().flex_wrap().items_center().gap(px(8.));
        let column = |w: f32| div().w(px(w)).flex_none().flex().flex_col().gap(px(10.));
        let presences = [Presence::Online, Presence::Idle, Presence::Dnd, Presence::Offline];
        let label = |words: &'static str| div().type_micro().text_color(theme::text_faint()).child(words);
        // One row of each height, to compare with the row family.
        let conversation = |id: &'static str, name: &'static str, preview: &'static str, time: &'static str, badge: Option<gpui::Div>, unread: bool| {
            let weight = if unread { theme::SEMIBOLD } else { theme::REGULAR };
            let content = div()
                .gap(px(10.))
                .child(Avatar::named(name, 32.).presence(Presence::Online, theme::sidebar()).build())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .child(network_mark(protocol::Network::Discord, 14., theme::text_faint()))
                                .child(div().font_weight(weight).truncate().child(name))
                                .child(div().flex_1())
                                .child(div().type_micro().text_color(theme::text_faint()).child(time)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .child(div().flex_1().min_w_0().truncate().type_small().text_color(if unread { theme::text() } else { theme::text_soft() }).child(preview))
                                .children(badge),
                        ),
                );
            list_row(id, theme::row_height(theme::ROW_TWO), false, false, content)
        };
        let place = |id: &'static str, name: &'static str, right: Option<gpui::Div>, selected: bool, focused: bool| {
            let content = div().gap(px(8.)).child(glyph("icons/hash.svg", 16., theme::text_faint())).child(div().flex_1().child(name)).children(right);
            list_row(id, theme::row_height(theme::ROW_ONE), selected, focused, content)
        };
        let person = |id: &'static str, name: &'static str, context: &'static str| {
            let content = div()
                .gap(px(8.))
                .child(Avatar::named(name, 24.).presence(Presence::Online, theme::sidebar()).build())
                .child(div().child(name))
                .child(div().type_small().text_color(theme::text_faint()).child(format!("· {context}")));
            list_row(id, theme::row_height(theme::ROW_MID), false, false, content)
        };
        div()
            .size_full()
            .p(px(24.))
            .flex()
            .gap(px(24.))
            .bg(theme::canvas())
            .child(
                column(340.)
                    .child(label("Buttons"))
                    .child(
                        line()
                            .child(button("b1", ButtonKind::Primary, "Primary"))
                            .child(button("b2", ButtonKind::Secondary, "Secondary"))
                            .child(button("b3", ButtonKind::Ghost, "Ghost"))
                            .child(button("b4", ButtonKind::Danger, "Delete")),
                    )
                    .child(
                        line()
                            .child(button("b8", ButtonKind::Primary, "Accept").h(px(24.)))
                            .child(button("b9", ButtonKind::Secondary, "Decline").h(px(24.)))
                            .child(button_disabled("Sending…"))
                            .child(icon_button("b6", "icons/smile.svg", false))
                            .child(icon_button("b7", "icons/smile.svg", true)),
                    )
                    .child(label("Fields"))
                    .child(field(false).child(div().text_color(theme::text_faint()).child("Display name")))
                    .child(field(true).child("Wren Halloran"))
                    .child(search_field(false).child(div().flex_1().text_color(theme::text_faint()).child("Jump to…")).child(keycap("⌘K")))
                    .child(label("Avatars"))
                    .child(
                        line()
                            .children([16., 20., 24., 32., 36., 64.].map(|size| Avatar::named("Wren Halloran", size).build()))
                            .children(["Odalys Brandt", "Ines Varga", "Niko Aldana", "Tobin Reyes"].into_iter().zip(presences).map(
                                |(name, presence)| Avatar::named(name, 32.).presence(presence, theme::canvas()).build(),
                            )),
                    )
                    .child(label("Badges: mention · count · unread count · dot · accent dot · 99+"))
                    .child(
                        line()
                            .child(badge_mention(3))
                            .child(badge_count(12))
                            .child(badge_count_unread(3))
                            .child(badge_dot(8., theme::unread()))
                            .child(badge_dot(8., theme::mention()))
                            .child(badge_mention(120)),
                    )
                    .child(segmented().child(segment("s1", "All", true)).child(segment("s2", "Unread 11", false)).child(segment("s3", "Mentions 4", false)))
                    .child(label("Banners"))
                    .child(banner(Tone::Attention, "Discord wants you to verify your account.").child(banner_action("ba1", "Open Discord")))
                    .child(banner(Tone::Warning, "Slack is reconnecting. Showing what was saved at 10:42."))
                    .child(banner(Tone::Danger, "Slack signed you out of Harbor Labs.").child(banner_action("ba2", "Sign in")))
                    .child(banner(Tone::Keychain, crate::host::STORE_WAITING))
                    .child(toast(Some(("icons/circle-alert.svg", theme::danger())), "tinyclient quit unexpectedly and reopened.").child(banner_action("t1", "Copy report"))),
            )
            .child(
                column(340.)
                    .child(label("Rows: conversation 48 · place 28 · person 36"))
                    .child(
                        div()
                            .bg(theme::sidebar())
                            .py(px(8.))
                            .rounded(px(theme::R_CARD))
                            .child(section_header("Pinned", true))
                            .child(conversation("r1", "Tomasz Brandt", "Ines says 7:30 at the usual corner.", "9:24 PM", None, false))
                            .child(conversation("r2", "Ines Marchetti", "And tell Tomasz, he never reads the channel", "10:27 PM", Some(badge_count_unread(3)), true))
                            .child(section_header("Today", false))
                            .child(conversation("r3", "#general", "Ines: need a yes or no on Saturday", "10:30 PM", Some(badge_mention(2)), true))
                            .child(section_header_folding("Meetings", false, true, Some(badge_dot(8., theme::unread()))))
                            .child(place("p1", "trail-cams", None, false, false))
                            .child(place("p2", "gear-swap", Some(badge_dot(8., theme::unread())), false, false))
                            .child(place("p3", "showcase", None, true, false))
                            .child(place("p4", "photos", None, false, true))
                            .child(person("q1", "Anneke Visser", "Harbor Labs"))
                            .child(person("q2", "Dita Novak", "Moderator")),
                    ),
            )
            .child(
                column(300.)
                    .child(label("Menu"))
                    .child(
                        menu()
                            .w(px(240.))
                            .child(menu_item("m0", Some("icons/reply.svg"), "Reply", Some("R")))
                            .child(menu_item("m1", Some(NO_GLYPH), "Reply in thread", Some("T")))
                            .child(menu_item_check("m2", true, "Online", None))
                            .child(menu_item_check("m3", false, "Idle", None))
                            .child(menu_item_disabled("m4", Some(NO_GLYPH), "Mark Server as Read", Some("⇧⎋")))
                            .child(menu_separator())
                            .child(menu_label("Status"))
                            .child(menu_item("m5", Some("icons/copy.svg"), "Copy text", Some("⌘C")))
                            .child(menu_separator())
                            .child(menu_item_danger("m6", Some("icons/trash-2.svg"), "Delete…")),
                    )
                    .child(label("Modal"))
                    .child(
                        modal("Delete this message?")
                            .w(px(300.))
                            .child(modal_body("It is deleted for everyone in #showcase."))
                            .child(modal_buttons().child(button("mb1", ButtonKind::Secondary, "Cancel")).child(button("mb2", ButtonKind::Danger, "Delete"))),
                    )
                    .child(label("Empty state"))
                    .child(
                        div().h(px(200.)).bg(theme::sidebar()).rounded(px(theme::R_CARD)).child(empty_state_with(
                            "icons/key.svg",
                            "tinyclient can't read its sign-ins",
                            "Your accounts are still signed in. Allow access to continue.",
                            button("es", ButtonKind::Secondary, "Try again"),
                        )),
                    ),
            )
    }

    pub fn shot(window: gpui::WindowHandle<Shell>, cx: &mut App) {
        let Some(path) = flag("--shot") else {
            return;
        };
        cx.spawn(async move |cx| {
            let settle = Duration::from_millis(500);
            cx.background_executor().timer(settle).await;
            for step in 0..3 {
                let _ = window.update(cx, |shell, window, cx| stage(shell, step, window, cx));
                cx.background_executor().timer(settle).await;
            }
            // Each press waits for what the one before it read.
            for _ in 0..flag("--walk").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0) {
                let _ = window.update(cx, |shell, _, cx| shell.walk_unread(true, cx));
                cx.background_executor().timer(settle).await;
            }
            for _ in 0..flag("--back").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0) {
                let _ = window.update(cx, |shell, _, cx| shell.travel(true, cx));
                cx.background_executor().timer(settle).await;
            }
            let frames = flag("--frames").and_then(|n| n.parse::<usize>().ok()).unwrap_or(1).max(1);
            if frames > 1 {
                super::anim::SHOT_FRAMES.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            for frame in 1..=frames {
                let named = match (frames, path.rsplit_once('.')) {
                    (1, _) => path.clone(),
                    (_, Some((stem, extension))) => format!("{stem}-{frame}.{extension}"),
                    (_, None) => format!("{path}-{frame}"),
                };
                // A frame drawn now, as a moving picture's timer would have the window drawn.
                let _ = window.update(cx, |_, window, _| window.refresh());
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let image = window.update(cx, |_, window, _| window.render_to_image());
                match image {
                    Ok(Ok(image)) => image.save(&named).expect("the screenshot is written"),
                    other => eprintln!("tinyclient: no screenshot: {:?}", other.map(|r| r.map(|_| ()))),
                }
            }
            // `--hold 20` keeps the window up so its memory can be measured from outside.
            if let Some(seconds) = flag("--hold").and_then(|n| n.parse::<u64>().ok()) {
                eprintln!("tinyclient: holding, pid {}", std::process::id());
                cx.background_executor().timer(Duration::from_secs(seconds)).await;
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    }
}

fn flag(name: &str) -> Option<String> {
    std::env::args().skip_while(|arg| arg != name).nth(1)
}

/// `--size 1120x720`, for drawing a state at a given width. `--narrow` is 700 wide.
fn window_size() -> (f32, f32) {
    let asked = flag("--size").and_then(|size| {
        let (wide, tall) = size.split_once('x')?;
        Some((wide.parse().ok()?, tall.parse().ok()?))
    });
    match asked {
        Some(size) => size,
        None if std::env::args().any(|arg| arg == "--narrow") => (700., 720.),
        None => (1120., 720.),
    }
}

/// When the process began drawing, for `--shot`'s report of how long the first list took.
#[cfg(feature = "gui-shot")]
static LAUNCHED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Where the window opens: `--size` for a shot, else the saved frame when half of it is on a
/// display that is still there, else 1120 × 720 centered on the main display.
fn window_bounds(saved: Option<(f32, f32, f32, f32)>, cx: &App) -> Bounds<gpui::Pixels> {
    let asked = flag("--size").is_some() || std::env::args().any(|arg| arg == "--narrow" || arg == "--shot");
    let displays: Vec<(f32, f32, f32, f32)> = cx
        .displays()
        .iter()
        .map(|display| {
            let b = display.bounds();
            (f32::from(b.origin.x), f32::from(b.origin.y), f32::from(b.size.width), f32::from(b.size.height))
        })
        .collect();
    if let Some((x, y, w, h)) = saved.filter(|frame| !asked && app::prefs::frame_visible(*frame, &displays)) {
        return Bounds { origin: point(px(x), px(y)), size: size(px(w), px(h)) };
    }
    let (wide, tall) = window_size();
    let mut bounds = Bounds::centered(None, size(px(wide), px(tall)), cx);
    // Centering in a visible area of odd size lands on half a point, and the system then grows
    // the frame to whole pixels: one point wider than asked.
    bounds.origin = point(bounds.origin.x.round(), bounds.origin.y.round());
    bounds
}

pub fn run() {
    #[cfg(feature = "gui-shot")]
    LAUNCHED.get_or_init(std::time::Instant::now);
    app::crash::install();
    app::read_reduce_motion();
    time::read_clock_preference();
    // Read before the window opens, so its first frame draws where the user was.
    let prefs = app::prefs::read();
    theme::set_text_step(prefs.text_step);
    app::prefs::set_choices(prefs.choices);
    anim::set_autoplay(prefs.autoplay);
    let application = gpui_platform::application().with_assets(assets::Assets);
    // The Dock icon brings the hidden window back unchanged.
    application.on_reopen(|cx| cx.activate(true));
    application.run(move |cx: &mut App| {
            input::bind_keys(cx);
            call::bind_keys(cx);
            bind_keys(cx);
            app::menubar::app_actions(cx);
            if !cfg!(feature = "gui-shot") {
                crate::update::recall();
            }
            cx.set_menus(app::menubar::menus(&[], None, crate::update::menu_label()));

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(window_bounds(prefs.frame, cx))),
                window_min_size: Some(size(px(layout::WINDOW_MIN.0), px(layout::WINDOW_MIN.1))),
                titlebar: Some(TitlebarOptions {
                    title: Some("tinyclient".into()),
                    appears_transparent: true,
                    // Centered on the 52 pt strips, inside the rail.
                    traffic_light_position: Some(point(px(10.), px(20.))),
                }),
                app_id: Some(APP_ID.into()),
                ..Default::default()
            };
            let opened = cx.open_window(options, |window, cx| {
                // The red button closes the window as ⌘W does (Keep receiving decides how).
                window.on_window_should_close(cx, |_, cx| {
                    app::menubar::close_window(cx);
                    false
                });
                cx.new(|cx| {
                    let shell = Shell::new(prefs, window, cx);
                    window.focus(&shell.focus, cx);
                    shell
                })
            });
            let window = match opened {
                Ok(window) => window,
                Err(err) => {
                    // No window (no GPU, a headless session): say so and leave, never panic.
                    eprintln!("tinyclient: the window could not open: {err}");
                    cx.quit();
                    return;
                }
            };
            #[cfg(feature = "gui-shot")]
            shot::shot(window, cx);
            #[cfg(feature = "gui-shot")]
            settings::shot(cx);
            let _ = window;
            cx.activate(true);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_typed_at_the_lists_go_to_the_composer_and_list_keys_do_not() {
        assert!(typed_text("a") && typed_text("É") && typed_text("?"));
        assert!(!typed_text("") && !typed_text("\n") && !typed_text("\t") && !typed_text(" "));
    }

}
