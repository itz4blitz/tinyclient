//! The sidebar: the same three parts in every mode (title strip, jump strip, one list). The
//! Inbox is by recency across networks, in day groups; a network's home is its own list
//! (Discord's DMs and servers, Slack's workspace, Telegram's chats, Teams' chats and teams); a
//! Space is its groups and channels in the network's own order; People is its own view in
//! this frame.

pub mod chat;
pub mod history;
pub mod hold;
pub mod home;
mod home_view;
pub mod inbox;
mod inbox_view;
pub mod pages;
pub mod rank;
#[cfg(feature = "gui-shot")]
mod shot;
pub mod switcher;
pub mod tree;
pub mod walk;

use super::components::{
    badge_dot, badge_mention, banner, banner_action, empty_state, glyph, icon_button, keycap, list_row, network_icon,
    pane_title, rollup_badge, row_badge, row_tone, segment, segmented, skeleton, tip, Avatar, Shape, Tone,
};
use super::layout::{drags_window, Menu};
use super::state::{Shell, Snapshot, SpaceTree, TROUBLE_SHOWN};
use super::theme::{self, Type};
use crate::client::ask;
use gpui::{
    div, prelude::*, px, uniform_list, Context, Div, FocusHandle, ListAlignment, ListState, MouseButton,
    ScrollStrategy, SharedString, Stateful, UniformListScrollHandle, Window,
};
use history::History;
use pages::Pages;
use protocol::{AccountKey, Badge, Conversation, ConversationKind, InboxCounts, InboxFilter, Key, Link, Network, Request, Response, PAGE_LIMIT};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;
use tree::Row;

/// The name this view watches people and rows under (see `people::Hub::watch`).
const WATCH: &str = "sidebar";
/// Drafts remembered ahead of their rows being read again.
const DRAFTS_HELD: usize = 256;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    Inbox,
    People,
    /// A network's home.
    Network(Network),
    Space(Key),
}

impl Mode {
    /// The network a mode is inside, for its banner.
    pub fn network(&self) -> Option<Network> {
        match self {
            Mode::Network(network) => Some(*network),
            Mode::Space(space) => Some(space.network),
            Mode::Inbox | Mode::People => None,
        }
    }
}

/// Somewhere the user has been.
#[derive(Clone, PartialEq, Eq)]
pub struct Location {
    mode: Mode,
    open: Option<(Key, String)>,
}

pub struct State {
    pub mode: Mode,
    pub filter: InboxFilter,
    /// The Inbox, held whole, in the order drawn.
    pub inbox: Pages<Rc<Conversation>>,
    /// How many rows each filter holds, as the last Inbox answer said.
    pub counts: InboxCounts,
    /// Whether the Inbox's order is being held (the pointer is over it, or the keys are in it).
    pub hold: hold::Hold,
    inbox_list: ListState,
    inbox_items: Rc<Vec<inbox::Item>>,
    apps_open: bool,
    apps_all: bool,
    /// The order that came in while the Inbox was held, and how many rows have news in it.
    waiting: Option<Vec<Rc<Conversation>>>,
    pill: usize,
    /// The newest conversation of the whole Inbox, for "You're all caught up".
    last_seen: Option<(String, i64)>,
    /// Muted conversations with new messages, as the last All answer had them: "You're all
    /// caught up" offers them, and lists them when asked.
    muted_news: Vec<Rc<Conversation>>,
    muted_open: bool,
    /// The network home on show: what chatd said, and the user's folds.
    pub home: home::Home,
    saved_homes: HashMap<Network, home::Home>,
    home_list: ListState,
    home_items: Rc<Vec<home::Item>>,
    /// The home's Unread segment, and how many rows it holds.
    home_unread: bool,
    home_news: u32,
    /// A list the home drilled into (Threads, Archive, Message requests, a forum's topics).
    pub drill: Option<home::Drill>,
    /// The Slack workspace on show, and whether its menu is open.
    pub slack: Option<Key>,
    slack_menu: bool,
    pub spaces_loaded: bool,
    /// Groups the user opened or closed against how they start, and what they revealed.
    opened: HashSet<String>,
    closed: HashSet<String>,
    quiet_shown: HashSet<String>,
    threads_shown: HashSet<Key>,
    muted_shown: HashSet<Key>,
    opted_shown: HashSet<Key>,
    /// The tree on show, flat. Rebuilt on collapse and when its data changes.
    pub rows: Rc<Vec<Row>>,
    tree_scroll: UniformListScrollHandle,
    /// The row the keyboard is on. Moving it opens nothing.
    pub cursor: Option<usize>,
    pub focus: FocusHandle,
    /// Spaces pinned to the rail, in its order.
    pub pins: Vec<Key>,
    history: History<Location>,
    /// Conversations the unread walk has opened since the user last opened one by hand.
    walked: Vec<Key>,
    /// The Space the unread walk is going into, and its direction, until its tree is read.
    walking: Option<(Key, bool)>,
    /// The sidebar's width as last drawn, for fitting a row's title.
    width: std::cell::Cell<f32>,
    /// Drafts typed since their rows were read: chatd announces no change for `SetDraft`.
    drafts: HashMap<Key, String>,
    /// Friend requests waiting, for People's dot on the rail.
    pub friend_requests: u32,
    /// A read of the home is on its way.
    pub home_pending: bool,
    pub switcher: Option<switcher::Switcher>,
}

fn list_state(cx: &mut Context<Shell>) -> ListState {
    let list = ListState::new(0, ListAlignment::Top, px(200.));
    // A scroll changes which rows are on screen, and so what is watched.
    let shell = cx.entity_id();
    list.set_scroll_handler(move |_, _, cx| cx.notify(shell));
    list
}

impl State {
    pub fn new(cx: &mut Context<Shell>) -> State {
        #[cfg(feature = "gui-shot")]
        shot::schedule(cx);
        State {
            mode: Mode::Inbox,
            filter: InboxFilter::All,
            inbox: Pages::default(),
            counts: InboxCounts::default(),
            hold: hold::Hold::default(),
            inbox_list: list_state(cx),
            inbox_items: Rc::new(Vec::new()),
            apps_open: false,
            apps_all: false,
            waiting: None,
            pill: 0,
            last_seen: None,
            muted_news: Vec::new(),
            muted_open: false,
            home: home::Home::default(),
            saved_homes: HashMap::new(),
            home_list: list_state(cx),
            home_items: Rc::new(Vec::new()),
            home_unread: false,
            home_news: 0,
            drill: None,
            slack: None,
            slack_menu: false,
            spaces_loaded: false,
            opened: HashSet::new(),
            closed: HashSet::new(),
            quiet_shown: HashSet::new(),
            threads_shown: HashSet::new(),
            muted_shown: HashSet::new(),
            opted_shown: HashSet::new(),
            rows: Rc::new(Vec::new()),
            tree_scroll: UniformListScrollHandle::new(),
            cursor: None,
            focus: cx.focus_handle(),
            pins: Vec::new(),
            history: History::default(),
            walked: Vec::new(),
            walking: None,
            width: std::cell::Cell::new(super::layout::SIDEBAR_WIDTH),
            drafts: HashMap::new(),
            friend_requests: 0,
            home_pending: false,
            switcher: None,
        }
    }

    /// Drops rows whose account is signed out. True when the Inbox changed, and when a home list did.
    pub fn without_signed_out(&mut self, keep: &HashSet<AccountKey>) -> (bool, bool) {
        let visible = |key: &Key| keep.contains(&key.account());
        let kept: Vec<_> = self.inbox.all().iter().filter(|row| visible(&row.key)).cloned().collect();
        let inbox_changed = kept.len() != self.inbox.all().len();
        if inbox_changed {
            let epoch = self.inbox.epoch();
            let total = kept.len() as u32;
            self.inbox.fill(epoch, kept, total);
            self.muted_news.retain(|row| visible(&row.key));
            if let Some(waiting) = self.waiting.as_mut() {
                waiting.retain(|row| visible(&row.key));
            }
        }
        let home = &mut self.home;
        let before = home.chats.len() + home.drilled.len() + home.in_voice.len() + home.archive.len();
        home.chats.retain(|row| visible(&row.key));
        home.drilled.retain(|row| visible(&row.key));
        home.in_voice.retain(|row| visible(&row.key));
        home.archive.retain(|row| visible(&row.key));
        let mut home_changed = home.chats.len() + home.drilled.len() + home.in_voice.len() + home.archive.len() != before;
        for saved in self.saved_homes.values_mut() {
            let before = saved.chats.len() + saved.drilled.len() + saved.in_voice.len() + saved.archive.len();
            saved.chats.retain(|row| visible(&row.key));
            saved.drilled.retain(|row| visible(&row.key));
            saved.in_voice.retain(|row| visible(&row.key));
            saved.archive.retain(|row| visible(&row.key));
            home_changed |= saved.chats.len() + saved.drilled.len() + saved.in_voice.len() + saved.archive.len() != before;
        }
        (inbox_changed, home_changed)
    }

    /// Every row the sidebar holds, wherever it is drawn.
    pub fn held(&self) -> impl Iterator<Item = &Rc<Conversation>> {
        let home = &self.home;
        self.inbox.all().iter().chain(&home.chats).chain(&home.drilled).chain(&home.in_voice).chain(&home.archive)
    }

    /// A conversation's row, if any list on show holds it.
    pub fn find(&self, trees: &HashMap<Key, SpaceTree>, key: &Key) -> Option<Conversation> {
        let in_trees = trees.values().flat_map(|tree| &tree.conversations);
        self.held().chain(in_trees).find(|row| &row.key == key).map(|row| (**row).clone())
    }

    /// The list on show is a GPUI `list` (the Inbox, a home) rather than a tree.
    fn listed(&self) -> bool {
        match &self.mode {
            Mode::Inbox => true,
            Mode::Network(Network::Slack) => self.drill.is_some(),
            Mode::Network(_) => true,
            Mode::People | Mode::Space(_) => false,
        }
    }
}

/// The glyph of a conversation in a tree. A channel only invited people see shows a lock.
pub fn kind_glyph(kind: ConversationKind, private: bool) -> &'static str {
    match kind {
        ConversationKind::Channel if private => "icons/lock.svg",
        ConversationKind::Direct => "icons/user.svg",
        ConversationKind::Group => "icons/users.svg",
        ConversationKind::Channel => "icons/hash.svg",
        ConversationKind::Announcement => "icons/megaphone.svg",
        ConversationKind::Voice => "icons/volume-2.svg",
        ConversationKind::Forum => "icons/messages-square.svg",
        ConversationKind::Thread => "icons/corner-down-right.svg",
    }
}

fn row_id(prefix: &'static str, key: &Key) -> gpui::ElementId {
    // The id carries the network and account, so two of them may reuse a conversation id.
    (gpui::ElementId::from(prefix), format!("{}/{}/{}", key.network.label(), key.account, key.id)).into()
}

/// The rows of a mixed-height list that are on screen, from its scroll position and the known
/// height of each item: what is watched and fetched for.
pub(in crate::gui) fn on_screen(list: &ListState, count: usize, height: impl Fn(usize) -> f32) -> Range<usize> {
    let top = list.logical_scroll_top();
    let tall = f32::from(list.viewport_bounds().size.height);
    // Before the first layout the list has no bounds: a window's worth.
    let tall = if tall <= 0. { 900. } else { tall };
    let start = top.item_ix.min(count);
    let mut y = -f32::from(top.offset_in_item);
    let mut end = start;
    while end < count && y < tall {
        y += height(end);
        end += 1;
    }
    start..end
}

/// Replaces a list's items, keeping the first visible item where it is when it is still there.
pub(in crate::gui) fn respliced<T>(list: &ListState, old: &[T], new: &[T], anchor: impl Fn(&T) -> Option<String>) {
    let top = list.logical_scroll_top();
    // A list at its top stays at its top: what is new there is what the user should see.
    if top.item_ix == 0 && top.offset_in_item <= px(0.) {
        list.splice(0..old.len(), new.len());
        list.scroll_to(gpui::ListOffset::default());
        return;
    }
    let kept = old.get(top.item_ix).and_then(&anchor).and_then(|key| new.iter().position(|item| anchor(item).as_ref() == Some(&key)));
    list.splice(0..old.len(), new.len());
    if let Some(ix) = kept {
        list.scroll_to(gpui::ListOffset { item_ix: ix, offset_in_item: top.offset_in_item });
    } else if top.item_ix > 0 {
        list.scroll_to(gpui::ListOffset { item_ix: top.item_ix.min(new.len()), offset_in_item: px(0.) });
    }
}

impl Shell {
    /// For the composer: what is typed and unsent in a conversation now, empty when nothing is.
    pub(super) fn draft_changed(&mut self, conversation: &Key, text: &str, cx: &mut Context<Self>) {
        let drafts = &mut self.sidebar_state.drafts;
        if drafts.len() >= DRAFTS_HELD && !drafts.contains_key(conversation) {
            drafts.clear();
        }
        if drafts.insert(conversation.clone(), text.trim().to_owned()).as_deref() != Some(text.trim()) {
            cx.notify();
        }
    }

    /// The draft a row shows: what the composer last said, else what chatd stored.
    fn draft_of<'a>(&'a self, c: &'a Conversation) -> &'a str {
        self.sidebar_state.drafts.get(&c.key).map_or(c.draft.as_str(), String::as_str)
    }

    // Where the sidebar is.

    /// Goes to a mode, remembering where the user was.
    pub(super) fn go(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.sidebar_state.mode == mode {
            let state = &mut self.sidebar_state;
            // Clicking the lit place leaves a drill and scrolls its list to the top.
            if state.drill.take().is_some() {
                self.rebuild_rows(cx);
            }
            let state = &self.sidebar_state;
            state.inbox_list.scroll_to(gpui::ListOffset::default());
            if !matches!(&state.mode, Mode::Space(space) if space.network == Network::Discord) {
                state.home_list.scroll_to(gpui::ListOffset::default());
            }
            state.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
            return;
        }
        self.remember();
        self.set_mode(mode, cx);
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        let state = &mut self.sidebar_state;
        if state.mode == Mode::Inbox && state.hold.release() {
            if let Some(fresh) = state.waiting.take() {
                let total = fresh.len() as u32;
                let epoch = state.inbox.epoch();
                state.inbox.fill(epoch, fresh, total);
            }
        }
        let keep_discord = state.mode.network() == Some(Network::Discord) && mode.network() == Some(Network::Discord);
        let home_network = match &mode {
            Mode::Space(space) if space.network == Network::Discord => Some(Network::Discord),
            Mode::Network(network) => Some(*network),
            _ => None,
        };
        state.home.switch_network(home_network, &mut state.saved_homes);
        state.mode = mode;
        state.cursor = None;
        state.drill = None;
        state.slack_menu = false;
        if !keep_discord {
            state.home_unread = false;
        }
        state.apps_open = false;
        state.apps_all = false;
        state.pill = 0;
        state.tree_scroll = UniformListScrollHandle::new();
        state.hold.reset();
        // The rows on show name who is watched again as they draw.
        let hub = self.people_state.hub();
        hub.watch(WATCH, [], cx);
        hub.watch_rows(WATCH, [], cx);
        self.show_mode(cx);
        self.rebuild_inbox(cx);
        // Navigation reads immediately; only background event bursts are debounced.
        self.load_home(cx);
    }

    fn here(&self) -> Location {
        let open = self.snapshot.as_ref().and_then(|s| s.open.as_ref());
        Location { mode: self.sidebar_state.mode.clone(), open: open.map(|open| (open.key.clone(), open.title.clone())) }
    }

    /// Notes where the user is, before they go somewhere else.
    pub(super) fn remember(&mut self) {
        let here = self.here();
        self.sidebar_state.history.leave(here);
    }

    /// `cmd-[` and `cmd-]`: replays where the user has been.
    pub(super) fn travel(&mut self, back: bool, cx: &mut Context<Self>) {
        let here = self.here();
        let Some(there) = self.sidebar_state.history.travel(back, here) else {
            return;
        };
        if self.sidebar_state.mode != there.mode {
            self.set_mode(there.mode, cx);
        }
        match there.open {
            Some((key, title)) if self.open.as_ref() != Some(&key) => self.open_now(key, &title, cx),
            _ => cx.notify(),
        }
    }

    /// Whether back (or forward) leads anywhere: the strip's buttons dim when it does not.
    pub(super) fn can_travel(&self, back: bool) -> bool {
        self.sidebar_state.history.can(back)
    }

    /// `cmd-up`: out of a drill, a Space's network, a network's Inbox.
    pub(super) fn up_level(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_state.drill.take().is_some() {
            self.rebuild_rows(cx);
            return;
        }
        match self.sidebar_state.mode.clone() {
            Mode::Space(space) if space.network == Network::Slack => self.go(Mode::Network(Network::Slack), cx),
            Mode::Space(space) => self.go(Mode::Network(space.network), cx),
            Mode::Network(_) | Mode::People => self.go(Mode::Inbox, cx),
            Mode::Inbox => {}
        }
    }

    /// The Slack workspace the home shows: the one last used, else the first.
    pub(super) fn slack_space(&self) -> Option<Key> {
        let slack: Vec<&protocol::Space> = self.spaces.iter().filter(|s| s.key.network == Network::Slack).collect();
        let kept = self.sidebar_state.slack.as_ref().filter(|key| slack.iter().any(|s| &s.key == *key));
        kept.cloned().or_else(|| slack.first().map(|s| s.key.clone()))
    }

    /// Makes the trees held match what the mode shows: one Space, Slack's workspace, or the
    /// teams of the Teams home that are open or need me. The rest are dropped; chatd holds them.
    pub(super) fn show_mode(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<Key> = match &self.sidebar_state.mode {
            Mode::Space(space) => vec![space.clone()],
            Mode::Network(Network::Slack) => self.slack_space().into_iter().collect(),
            Mode::Network(Network::Teams) => self
                .spaces
                .iter()
                .filter(|s| s.key.network == Network::Teams)
                .filter(|s| s.attention > 0 || s.unread || self.sidebar_state.home.opened.contains(&s.key.id) || self.open.as_ref().is_some_and(|open| open.account() == s.key.account() && self.trees.get(&s.key).is_some_and(|t| t.conversations.iter().any(|c| &c.key == open))))
                .map(|s| s.key.clone())
                .collect(),
            Mode::Network(_) | Mode::Inbox | Mode::People => Vec::new(),
        };
        // One Space's live subscription at a time: the one on show.
        let watched = match &self.sidebar_state.mode {
            Mode::Space(space) => Some(space.clone()),
            Mode::Network(Network::Slack) => wanted.first().cloned(),
            _ => None,
        };
        self.people_state.hub().watch_space(watched.as_ref(), cx);
        self.trees.retain(|space, _| wanted.contains(space));
        for space in wanted {
            if !self.trees.contains_key(&space) {
                self.load_tree(space, cx);
            }
        }
        self.rebuild_rows(cx);
    }

    /// The tree on show as flat rows, or the home's list.
    pub(super) fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let state = &self.sidebar_state;
        let flat = |space: &Key, depth: u8| match self.trees.get(space).filter(|tree| tree.loaded) {
            Some(tree) => {
                let hide_muted = self.spaces.iter().any(|s| &s.key == space && s.hide_muted);
                let rules = tree::Rules {
                    network: Some(space.network),
                    hide_muted,
                    muted_shown: state.muted_shown.contains(space),
                    opted_out_shown: state.opted_shown.contains(space),
                    opened: Some(&state.opened),
                    closed: Some(&state.closed),
                    quiet_shown: Some(&state.quiet_shown),
                    threads_shown: Some(&state.threads_shown),
                    unread: state.home_unread && space.network == Network::Slack,
                };
                tree::flatten(space, &tree.groups, &tree.conversations, self.open.as_ref(), depth, &rules)
            }
            None => vec![Row::Loading { depth }; 6],
        };
        let rows = match &state.mode {
            Mode::Space(space) => flat(space, 0),
            Mode::Network(Network::Slack) => match self.slack_space() {
                Some(space) => {
                    let threads = self.threads_row(&space);
                    threads.into_iter().chain(flat(&space, 0)).collect()
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        };
        if *self.sidebar_state.rows != rows {
            self.sidebar_state.rows = Rc::new(rows);
        }
        self.rebuild_home(cx);
        self.walk_arrived(cx);
        cx.notify();
    }

    // What rows do.

    /// Collapses or opens a Group. chatd remembers it; the network never hears.
    fn collapse(&mut self, space: Key, group: String, collapsed: bool, cx: &mut Context<Self>) {
        let state = &mut self.sidebar_state;
        if collapsed {
            state.opened.remove(&group);
            state.closed.insert(group.clone());
        } else {
            state.closed.remove(&group);
            state.opened.insert(group.clone());
        }
        if group == tree::APPS_GROUP {
            self.rebuild_rows(cx);
            return;
        }
        if let Some(found) = self.trees.get_mut(&space).and_then(|tree| tree.groups.iter_mut().find(|g| g.id == group)) {
            found.collapsed = collapsed;
        }
        self.rebuild_rows(cx);
        if super::people::only_draws() {
            return;
        }
        let request = Request::Collapse { space, group, collapsed };
        self.off_thread(cx, move || ask(&request), |_, _, _| {});
    }

    pub(super) fn star(&mut self, conversation: Key, starred: bool, cx: &mut Context<Self>) {
        let request = Request::Star { conversation, starred };
        self.off_thread(cx, move || ask(&request), |this, _, cx| {
            let pages = this.sidebar_state.inbox.stale();
            this.load_inbox(pages, cx);
        });
    }

    pub(super) fn pin(&mut self, space: Key, pinned: bool, cx: &mut Context<Self>) {
        let pins = &mut self.sidebar_state.pins;
        pins.retain(|pin| pin != &space);
        if pinned {
            pins.push(space);
        }
        self.save_prefs(cx);
        cx.notify();
    }

    pub(super) fn set_filter(&mut self, filter: InboxFilter, cx: &mut Context<Self>) {
        if self.sidebar_state.filter == filter {
            return;
        }
        let state = &mut self.sidebar_state;
        state.filter = filter;
        state.cursor = None;
        state.waiting = None;
        state.pill = 0;
        state.muted_open = false;
        state.hold.reset();
        state.inbox.reset();
        self.rebuild_inbox(cx);
        self.load_inbox(vec![0], cx);
        cx.notify();
    }

    /// What Return or a click does to the tree row at `ix`.
    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.sidebar_state.rows.get(ix).cloned() else {
            return;
        };
        match row {
            Row::Conversation { conversation, .. } => self.pick_row(&conversation, cx),
            Row::Group { space, id, collapsed, .. } => self.collapse(space, id, !collapsed, cx),
            Row::Threads { space, .. } => self.drill(home::Drill::Threads(space), cx),
            Row::Fold { reveal, .. } => {
                let space = match &self.sidebar_state.mode {
                    Mode::Space(space) => Some(space.clone()),
                    _ => self.slack_space(),
                };
                let state = &mut self.sidebar_state;
                match (reveal, space) {
                    (tree::Reveal::Muted, Some(space)) => {
                        state.muted_shown.insert(space);
                    }
                    (tree::Reveal::OptedOut, Some(space)) => {
                        state.opted_shown.insert(space);
                    }
                    (tree::Reveal::Threads(channel), _) => {
                        if !state.threads_shown.remove(&channel) {
                            state.threads_shown.insert(channel);
                        }
                    }
                    (tree::Reveal::Quiet(group), _) => {
                        state.quiet_shown.insert(group);
                    }
                    _ => return,
                }
                self.rebuild_rows(cx);
            }
            Row::Occupant { occupant, .. } => {
                if let Some(person) = self.people_state.hub().person(&occupant.person) {
                    self.open_person(&person, cx);
                }
            }
            Row::More { .. } | Row::Loading { .. } => {}
        }
    }

    fn list_len(&self) -> usize {
        let state = &self.sidebar_state;
        match state.mode {
            Mode::Inbox => state.inbox_items.len(),
            Mode::People => 0,
            _ if state.listed() => state.home_items.len(),
            _ => state.rows.len(),
        }
    }

    /// Whether the keyboard may stop on the item at `ix` of the list on show.
    fn stoppable(&self, ix: usize) -> bool {
        let state = &self.sidebar_state;
        match state.mode {
            Mode::Inbox => state.inbox_items.get(ix).is_some_and(|item| !matches!(item, inbox::Item::Header { .. } | inbox::Item::End)),
            _ if state.listed() => state.home_items.get(ix).is_some_and(|item| !matches!(item, home::Item::Header { .. })),
            _ => state.rows.get(ix).is_some_and(|row| !matches!(row, Row::Loading { .. } | Row::More { .. } | Row::Occupant { .. })),
        }
    }

    fn scroll_to(&mut self, ix: usize) {
        let state = &self.sidebar_state;
        match state.mode {
            Mode::Inbox => state.inbox_list.scroll_to_reveal_item(ix),
            _ if state.listed() => state.home_list.scroll_to_reveal_item(ix),
            _ => state.tree_scroll.scroll_to_item(ix, ScrollStrategy::Nearest),
        }
    }

    /// Arrow keys in the list: the cursor moves, nothing opens.
    pub(super) fn move_cursor(&mut self, down: bool, cx: &mut Context<Self>) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let from = self.sidebar_state.cursor;
        let mut steps: Box<dyn Iterator<Item = usize>> = match (from, down) {
            (None, true) => Box::new(0..len),
            (None, false) => Box::new((0..len).rev()),
            (Some(at), true) => Box::new(at + 1..len),
            (Some(at), false) => Box::new((0..at).rev()),
        };
        let Some(next) = steps.find(|ix| self.stoppable(*ix)).or(from) else {
            return;
        };
        self.sidebar_state.cursor = Some(next);
        self.sidebar_state.hold.key_moved(Instant::now());
        self.settle_later(super::sidebar::hold::KEY_HOLD, cx);
        self.scroll_to(next);
        cx.notify();
    }

    /// The conversation of the list item at `ix`, in whichever list is on show.
    fn listed_at(&self, ix: usize) -> Option<Rc<Conversation>> {
        let state = &self.sidebar_state;
        match state.mode {
            Mode::Inbox => state.inbox_items.get(ix)?.conversation().cloned(),
            _ if state.listed() => state.home_items.get(ix)?.conversation().cloned(),
            _ => state.rows.get(ix)?.conversation().cloned(),
        }
    }

    /// Where the open conversation (or else the cursor) is in the list on show.
    fn list_at(&self) -> Option<usize> {
        let open = self.open.as_ref();
        let at = open.and_then(|open| (0..self.list_len()).find(|ix| self.listed_at(*ix).is_some_and(|c| &c.key == open)));
        at.or(self.sidebar_state.cursor)
    }

    /// Puts the cursor on the open conversation's row, or the first row, when it is nowhere.
    pub(super) fn cursor_to_open(&mut self) {
        if self.sidebar_state.cursor.is_some() {
            return;
        }
        let at = self.list_at().or_else(|| (0..self.list_len()).find(|ix| self.stoppable(*ix)));
        self.sidebar_state.cursor = Some(at.unwrap_or(0));
    }

    /// Return on the cursor's row.
    pub(super) fn open_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.sidebar_state.cursor else {
            return;
        };
        let state = &self.sidebar_state;
        match state.mode {
            Mode::Inbox => self.activate_inbox(ix, cx),
            _ if state.listed() => self.activate_home(ix, cx),
            _ => self.activate(ix, cx),
        }
    }

    /// Opens the conversation at `ix` of the list on show, as a click on it would.
    #[cfg_attr(not(feature = "gui-shot"), allow(dead_code))]
    pub(super) fn open_listed(&mut self, ix: usize, cx: &mut Context<Self>) {
        let rows: Vec<Rc<Conversation>> = (0..self.list_len()).filter_map(|at| self.listed_at(at)).collect();
        if let Some(row) = rows.get(ix) {
            self.select(row.key.clone(), &row.title, cx);
        }
    }

    /// Left and right on the cursor's row: collapse or open a group.
    pub(super) fn fold_cursor(&mut self, open: bool, cx: &mut Context<Self>) {
        let state = &self.sidebar_state;
        if state.listed() || state.mode == Mode::Inbox {
            return;
        }
        if let Some(Row::Group { space, id, collapsed, .. }) = state.cursor.and_then(|ix| state.rows.get(ix)).cloned() {
            if collapsed == open {
                self.collapse(space, id, !open, cx);
            }
        }
    }

    /// Opens a row the user chose by hand: the unread walk starts over from there.
    fn pick_row(&mut self, row: &Conversation, cx: &mut Context<Self>) {
        self.sidebar_state.walked.clear();
        if row.key.network == Network::Discord && row.kind.is_direct() && matches!(&self.sidebar_state.mode, Mode::Space(space) if space.network == Network::Discord) {
            self.go(Mode::Network(Network::Discord), cx);
        }
        if row.kind == ConversationKind::Forum && row.key.network == Network::Telegram {
            self.drill(home::Drill::Topics(Rc::new(row.clone())), cx);
            return;
        }
        self.select(row.key.clone(), &row.title, cx);
    }

    /// `alt-down` and `alt-up` open the next or previous row of the list on show.
    pub(super) fn walk(&mut self, down: bool, cx: &mut Context<Self>) {
        let len = self.list_len();
        let at = self.list_at();
        let mut steps: Box<dyn Iterator<Item = usize>> = match (at, down) {
            (None, _) => Box::new(0..len),
            (Some(at), true) => Box::new(at + 1..len),
            (Some(at), false) => Box::new((0..at).rev()),
        };
        let found = steps.find_map(|ix| self.listed_at(ix).map(|row| (ix, row)));
        if let Some((ix, row)) = found {
            self.sidebar_state.cursor = Some(ix);
            self.scroll_to(ix);
            self.pick_row(&row, cx);
        }
    }

    /// The stops of the unread walk: the Inbox, then every Space in the rail's network order.
    fn walk_stops(&self) -> Vec<(Mode, bool)> {
        let counts = self.sidebar_state.counts;
        let inbox = (Mode::Inbox, counts.unread > 0 || counts.mentions > 0);
        let spaces = super::layout::NETWORK_ORDER.into_iter().flat_map(|network| self.spaces.iter().filter(move |space| space.key.network == network));
        std::iter::once(inbox).chain(spaces.map(|space| (Mode::Space(space.key.clone()), space.attention > 0 || space.unread))).collect()
    }

    /// `alt-shift-down` and `alt-shift-up`: the next or previous unread conversation. Past the
    /// end of the list on show it goes on into the next place with something unread, the
    /// sidebar following, until nothing unread is left.
    pub(super) fn walk_unread(&mut self, down: bool, cx: &mut Context<Self>) {
        self.sidebar_state.walking = None;
        match self.sidebar_state.mode.clone() {
            Mode::Inbox => self.walk_inbox(down, true, cx),
            Mode::Space(_) | Mode::Network(Network::Slack) => {
                let rows = self.sidebar_state.rows.clone();
                match self.tree_unread(&rows, self.list_at(), down) {
                    Some(ix) => self.walk_to_row(ix, cx),
                    None => self.walk_on(down, cx),
                }
            }
            Mode::Network(_) | Mode::People => self.walk_on(down, cx),
        }
    }

    /// The next unread row of a tree that this walk has not already opened.
    fn tree_unread(&self, rows: &[Row], from: Option<usize>, down: bool) -> Option<usize> {
        let mut at = from;
        loop {
            let ix = tree::walk(rows, at, down, true)?;
            let fresh = rows[ix].conversation().is_some_and(|c| !self.sidebar_state.walked.contains(&c.key));
            if fresh {
                return Some(ix);
            }
            at = Some(ix);
        }
    }

    fn walk_to_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.sidebar_state.rows.get(ix).and_then(|row| row.conversation()).cloned() else {
            return;
        };
        self.sidebar_state.cursor = Some(ix);
        self.scroll_to(ix);
        self.walk_open(&row, cx);
    }

    fn walk_open(&mut self, row: &Conversation, cx: &mut Context<Self>) {
        let walked = &mut self.sidebar_state.walked;
        if walked.len() >= PAGE_LIMIT as usize {
            walked.clear();
        }
        walked.push(row.key.clone());
        self.select(row.key.clone(), &row.title, cx);
    }

    /// The Inbox's unread rows are read from the store, not from what is drawn: the next one
    /// may be anywhere. `onward`: with none left here, go on to the next stop.
    fn walk_inbox(&mut self, down: bool, onward: bool, cx: &mut Context<Self>) {
        let request = Request::Inbox { network: None, filter: InboxFilter::Unread, offset: 0, limit: PAGE_LIMIT };
        self.off_thread(cx, move || ask(&request), move |this, answer, cx| {
            let Ok(Response::Conversations { conversations, .. }) = answer else {
                return;
            };
            let seen: Vec<bool> = conversations.iter().map(|c| this.sidebar_state.walked.contains(&c.key) || c.bot).collect();
            let at = this.open.as_ref().and_then(|open| conversations.iter().position(|c| &c.key == open));
            match walk::step(&seen, at, down) {
                Some(ix) => {
                    if this.sidebar_state.mode != Mode::Inbox {
                        this.remember();
                        this.set_mode(Mode::Inbox, cx);
                    }
                    let key = &conversations[ix].key;
                    let held = (0..this.sidebar_state.inbox_items.len()).find(|at| this.listed_at(*at).is_some_and(|c| &c.key == key));
                    this.sidebar_state.cursor = held;
                    if let Some(held) = held {
                        this.scroll_to(held);
                    }
                    this.walk_open(&conversations[ix], cx);
                }
                None if onward => this.walk_on(down, cx),
                None => {}
            }
        });
    }

    /// Nothing unread is left in the list on show: on to the next stop that has some.
    fn walk_on(&mut self, down: bool, cx: &mut Context<Self>) {
        let stops = self.walk_stops();
        let here = match &self.sidebar_state.mode {
            Mode::Space(_) => stops.iter().position(|(mode, _)| *mode == self.sidebar_state.mode).unwrap_or(0),
            // A network home walks on from the Space before its first, so that one comes next.
            Mode::Network(network) => stops
                .iter()
                .position(|(mode, _)| matches!(mode, Mode::Space(space) if space.network == *network))
                .map_or(0, |first| first.saturating_sub(usize::from(down))),
            Mode::Inbox | Mode::People => 0,
        };
        let unread: Vec<bool> = stops.iter().map(|(_, unread)| *unread).collect();
        // The nearest stop only: if it turns out to hold nothing new, the next key goes on.
        let Some(next) = walk::route(&unread, here, down).into_iter().find(|stop| stops[*stop].0 != self.sidebar_state.mode) else {
            return;
        };
        match stops[next].0.clone() {
            Mode::Space(space) => {
                self.remember();
                self.sidebar_state.walking = Some((space.clone(), down));
                self.set_mode(Mode::Space(space), cx);
            }
            _ => self.walk_inbox(down, false, cx),
        }
    }

    /// The tree the walk went into has been read: open its first (or last) unread row.
    fn walk_arrived(&mut self, cx: &mut Context<Self>) {
        let Some((space, down)) = self.sidebar_state.walking.clone() else {
            return;
        };
        if self.sidebar_state.mode != Mode::Space(space.clone()) {
            self.sidebar_state.walking = None;
            return;
        }
        if !self.trees.get(&space).is_some_and(|tree| tree.loaded) {
            return;
        }
        self.sidebar_state.walking = None;
        let rows = self.sidebar_state.rows.clone();
        if let Some(ix) = self.tree_unread(&rows, None, down) {
            self.walk_to_row(ix, cx);
        }
    }

    // Drawing.

    /// The people whose presence these rows draw, and the rows themselves for their previews.
    fn watch_rows<'a>(&self, rows: impl Iterator<Item = &'a Rc<Conversation>> + Clone, cx: &mut Context<Self>) {
        let hub = self.people_state.hub();
        let home = &self.sidebar_state.home_items;
        let visible = on_screen(&self.sidebar_state.home_list, home.len(), |ix| home[ix].height());
        let extra = home[visible].iter().filter_map(home::Item::conversation).filter(|_| matches!(&self.sidebar_state.mode, Mode::Space(space) if space.network == Network::Discord));
        let rows = rows.cloned().chain(extra.cloned());
        let peers = rows.clone().filter_map(|c| Some(Key { id: c.peer.clone()?, ..c.key.clone() }));
        hub.watch(WATCH, peers, cx);
        hub.watch_rows(WATCH, rows.map(|c| c.key.clone()), cx);
    }

    /// One row of a tree: 28 tall, whatever it is.
    fn tree_row(&self, ix: usize, row: &Row, focused: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        // A spacer rather than padding: the row sets its own.
        // 16 a level, less the row's gap, which follows it; none at the top, so a top-level
        // row starts on the 16 pt text edge (C10) with the Threads row and the headers.
        let indent = |depth: u8, gap: f32| (depth > 0).then(|| div().flex_none().w(px(16. * f32::from(depth) - gap)));
        let height = theme::row_height(theme::ROW_ONE);
        match row {
            Row::Loading { depth } => {
                let widths = [0.60, 0.45, 0.70, 0.50, 0.65, 0.40];
                let bar = div().h(px(8.)).w(gpui::relative(widths[ix % widths.len()])).rounded(px(theme::R_CHIP)).bg(theme::raised());
                list_row(("loading", ix), height, false, false, div().children(indent(*depth, 8.)).gap(px(8.)).child(skeleton(20., 20.)).child(bar)).cursor_default()
            }
            Row::Threads { count, mention, .. } => {
                let trailing = match (*count, *mention) {
                    (0, _) => None,
                    (count, true) => Some(badge_mention(count)),
                    (count, false) => Some(super::components::badge_count_unread(count)),
                };
                self.action_row(("threads", ix), Some("icons/messages-square.svg"), "Threads".into(), *count > 0, trailing, focused)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.activate(ix, cx)))
            }
            Row::Fold { words, depth, .. } => {
                let content = div()
                    .children(indent(*depth, 8.))
                    .gap(px(8.))
                    .child(div().size(px(20.)).flex_none().flex().items_center().justify_center().child(glyph("icons/ellipsis.svg", 14., theme::text_faint())))
                    .child(div().flex_1().min_w_0().truncate().type_ui().text_color(theme::text_faint()).child(words.clone()));
                list_row(("fold", ix), height, false, focused, content)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.activate(ix, cx)))
            }
            Row::More { words, depth } => {
                let content = div().children(indent(*depth, 0.)).child(div().pl(px(28.)).type_small().text_color(theme::text_faint()).child(words.clone()));
                list_row(("more", ix), height, false, false, content).cursor_default()
            }
            Row::Occupant { occupant, depth } => {
                let content = div()
                    .children(indent(*depth, 8.))
                    .gap(px(8.))
                    .child(Avatar::new(&occupant.person, &occupant.name, 20.).image_at(self.person_picture(&occupant.person, cx)).build())
                    .child(div().flex_1().min_w_0().truncate().type_ui().text_color(theme::text_soft()).child(occupant.name.clone()))
                    .when(occupant.streaming, |row| row.child(glyph("icons/monitor-up.svg", 14., theme::text_soft())))
                    .when(occupant.muted && !occupant.deafened, |row| row.child(glyph("icons/mic-off.svg", 14., theme::danger())))
                    .when(occupant.deafened, |row| row.child(glyph("icons/headphone-off.svg", 14., theme::danger())));
                list_row(row_id("occupant", &occupant.person), height, false, false, content)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.activate(ix, cx)))
            }
            Row::Group { id, name, collapsed, rollup, depth, .. } => {
                let lit = *collapsed && (rollup.attention > 0 || rollup.unread);
                let dot = rollup_badge(rollup.attention, rollup.unread).filter(|_| *collapsed);
                let chevron = if *collapsed { "icons/chevron-right.svg" } else { "icons/chevron-down.svg" };
                let group = SharedString::from(format!("group-{id}"));
                let content = div()
                    .group(group.clone())
                    .children(indent(*depth, 4.))
                    .gap(px(4.))
                    .child(div().min_w_0().truncate().type_micro().text_color(if lit { theme::text() } else { theme::text_faint() }).child(name.clone()))
                    .child(glyph(chevron, 12., theme::text_faint()).when(!*collapsed, |chevron| chevron.invisible().group_hover(group, |style| style.visible())))
                    .child(div().flex_1())
                    .children(dot);
                list_row((gpui::ElementId::from("group"), id.clone()), height, false, focused, content)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.activate(ix, cx)))
            }
            Row::Conversation { conversation: c, depth } => self.place_row(c, *depth, focused, cx),
        }
    }

    /// A conversation in a tree, 28: glyph or avatar 20, the name, then what is going on.
    fn place_row(&self, c: &Rc<Conversation>, depth: u8, focused: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let selected = self.open.as_ref() == Some(&c.key);
        let tone = row_tone(c, selected, true);
        let surface = theme::row_surface(selected);
        let occupied = c.occupants.as_ref().filter(|inside| inside.total > 0);
        // An empty voice channel rests at a muted row's weight (`02` §4).
        let empty_voice = c.kind == ConversationKind::Voice && occupied.is_none() && !selected;
        let title_color = if empty_voice { theme::text_faint() } else { tone.title };
        let leading = match c.kind {
            ConversationKind::Direct | ConversationKind::Group => self.conversation_avatar(c, 20., surface, cx).into_any_element(),
            kind => {
                let icon_color = if c.muted || empty_voice { theme::text_faint() } else { theme::text_soft() };
                let icon = if c.key.network == Network::Teams && kind == ConversationKind::Channel && !c.private { None } else { Some(kind_glyph(kind, c.private)) };
                div().size(px(20.)).flex_none().flex().items_center().justify_center().children(icon.map(|icon| glyph(icon, if kind == ConversationKind::Thread { 14. } else { 16. }, icon_color))).into_any_element()
            }
        };
        let status = c
            .peer
            .as_ref()
            .filter(|_| c.kind == ConversationKind::Direct && c.key.network == Network::Slack)
            .and_then(|peer| self.people_state.hub().person(&Key { id: peer.clone(), ..c.key.clone() }))
            .map(|person| person.emoji)
            .filter(|emoji| !emoji.is_empty());
        let limit = occupied.map(|inside| inside.total.to_string());
        let draft = (!self.draft_of(c).is_empty() && !selected).then(|| glyph("icons/pencil.svg", 14., theme::text_faint()));
        let (picked, menu_row) = (c.clone(), c.clone());
        let content = div()
            // 16 a level less the gap after it (none at the top), as the tree's other rows.
            .children((depth > 0).then(|| div().flex_none().w(px(16. * f32::from(depth) - 8.))))
            .gap(px(8.))
            .child(leading)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .type_ui()
                    .font_weight(tone.weight)
                    .text_color(title_color)
                    .child(tree_title(c)),
            )
            .children(status.map(|emoji| div().flex_none().type_small().child(emoji)))
            .children(chat::name_mark(c).filter(|_| c.kind.is_direct()))
            .child(div().flex_1())
            .children(draft)
            .children(limit.map(|total| div().flex_none().flex().items_center().gap(px(2.)).child(glyph("icons/volume-2.svg", 12., theme::text_faint())).child(div().type_micro().text_color(theme::text_faint()).child(total))))
            .children(row_badge(c));
        list_row(row_id("tree", &c.key), theme::row_height(theme::ROW_ONE), selected, focused, content)
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.pick_row(&picked, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.open_menu(Menu::Row(menu_row.clone(), event.position), cx);
                }),
            )
    }

    fn tree_rows(&mut self, range: Range<usize>, window: &mut Window, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let rows = self.sidebar_state.rows.clone();
        let visible = rows.get(range.start.min(rows.len())..range.end.min(rows.len())).unwrap_or_default();
        self.watch_rows(visible.iter().filter_map(|row| row.conversation()), cx);
        // The shell's own focus is the list's too: arrows move the cursor when no field has them.
        let focused = self.sidebar_state.focus.is_focused(window) || self.focus.is_focused(window);
        let cursor = self.sidebar_state.cursor.filter(|_| focused);
        range.filter_map(|ix| Some(self.tree_row(ix, rows.get(ix)?, cursor == Some(ix), cx))).collect()
    }

    /// What a pane says while a link is not normal. Healthy is silent, and a pane speaks only
    /// for the accounts it shows: a workspace's tree says nothing of another workspace. The
    /// window's own banner (chatd, the keychain) comes first; then the most urgent account's.
    fn banners(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Vec<Div> {
        if let Some(window) = self.window_banner(cx) {
            return vec![window];
        }
        let mode = &self.sidebar_state.mode;
        let mut found: Vec<(Network, Tone, String)> = Vec::new();
        for account in &snapshot.accounts {
            let network = account.key.network;
            let here = match mode {
                Mode::Space(space) => space.account() == account.key,
                Mode::Network(Network::Slack) => self.slack_space().is_none_or(|space| space.account() == account.key),
                Mode::Network(shown) => *shown == network,
                Mode::Inbox => true,
                Mode::People => false,
            };
            if !here {
                continue;
            }
            let words = banner_words(network, snapshot.accounts_of(network) > 1, account, *mode == Mode::Inbox, || {
                self.trouble.get(&account.key).map(|since| since.elapsed()).unwrap_or_default() >= TROUBLE_SHOWN
            });
            if let Some((tone, words)) = words {
                found.push((network, tone, words));
            }
        }
        // One per pane: the most urgent, not the first account's.
        found.sort_by_key(|(_, tone, _)| urgency(*tone));
        found
            .into_iter()
            .take(1)
            .map(|(network, tone, words)| match tone {
                Tone::Danger => banner(tone, words).child(
                    banner_action("sign-in-again", "Sign in").on_click(cx.listener(move |this, _, _, cx| this.sign_in(network, cx))),
                ),
                Tone::Attention => banner(tone, words).child(
                    banner_action("open-network", format!("Open {}", network.label())).on_click(move |_, _, cx| cx.open_url(network_app(network))),
                ),
                tone => banner(tone, words),
            })
            .collect()
    }

    /// `All · Unread n · Mentions n`: counts in faint numerals after the label, hidden at 0.
    fn segments(&self, choices: &[(&'static str, &'static str, u32, bool)], cx: &mut Context<Self>, pick: impl Fn(&mut Shell, usize, &mut Context<Shell>) + Clone + 'static) -> Div {
        let roomy = self.sidebar_state.width.get() >= 300.;
        let mut out = segmented();
        for (ix, (id, label, count, on)) in choices.iter().copied().enumerate() {
            let pick = pick.clone();
            let counted = (count > 0 && roomy).then(|| {
                let words = if count > 99 { "99+".to_owned() } else { count.to_string() };
                div().ml(px(5.)).text_size(px(11.)).font_weight(theme::MEDIUM).text_color(if on { theme::text() } else { theme::text_faint() }).child(words)
            });
            out = out.child(
                segment(id, label, on)
                    .children(counted)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| pick(this, ix, cx))),
            );
        }
        out
    }

    fn title_strip(&self, mode: &Mode, cx: &mut Context<Self>) -> Div {
        let strip = drags_window(div()).h(px(theme::STRIP)).flex_none().pl(px(theme::PANE_PAD)).pr(px(12.)).flex().items_center().gap(px(8.));
        let quiet = |button: Stateful<Div>| button.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        let back = |id: &'static str, words: String, cx: &mut Context<Self>| {
            quiet(icon_button(id, "icons/chevron-left.svg", false))
                .tooltip(tip(words, Some("⌘↑")))
                .on_click(cx.listener(|this, _, _, cx| this.up_level(cx)))
        };
        if let Some(drill) = &self.sidebar_state.drill {
            let title = match drill {
                home::Drill::Threads(_) => "Threads".to_owned(),
                home::Drill::Archive => "Archive".to_owned(),
                home::Drill::Requests => "Message requests".to_owned(),
                home::Drill::Topics(forum) => forum.title.clone(),
            };
            let place = match drill {
                home::Drill::Threads(space) => self.spaces.iter().find(|s| &s.key == space).map(|s| s.name.clone()),
                _ => None,
            };
            return strip
                .pl(px(10.))
                .child(back("drill-up", "Back".into(), cx))
                .child(pane_title(title))
                .children(place.map(|place| div().flex_none().max_w(px(120.)).truncate().type_small().text_color(theme::text_faint()).child(place)));
        }
        match mode {
            Mode::Inbox => {
                let filter = self.sidebar_state.filter;
                let counts = self.sidebar_state.counts;
                let choices = [
                    ("filter-all", "All", 0, filter == InboxFilter::All),
                    ("filter-unread", "Unread", counts.unread, filter == InboxFilter::Unread),
                    ("filter-mentions", "Mentions", counts.mentions, filter == InboxFilter::Mentions),
                ];
                let pick = |this: &mut Shell, ix: usize, cx: &mut Context<Shell>| {
                    this.set_filter([InboxFilter::All, InboxFilter::Unread, InboxFilter::Mentions][ix], cx);
                };
                strip.child(pane_title("Inbox")).child(self.segments(&choices, cx, pick))
            }
            Mode::People => strip.child(pane_title("People")),
            Mode::Network(Network::Slack) => self.slack_title(strip, cx),
            Mode::Network(network) => {
                let network = *network;
                let strip = strip.child(network_icon(network, 20.));
                match network {
                    Network::Discord => {
                        let unread = self.sidebar_state.home_unread;
                        let choices = [("home-all", "All", 0, !unread), ("home-unread", "Unread", self.sidebar_state.home_news, unread)];
                        let strip = strip.child(
                            div()
                                .id("network-name")
                                .flex_1()
                                .min_w_0()
                                .h(px(28.))
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .cursor_pointer()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, _, cx| this.open_menu(Menu::Network(network), cx)))
                                .child(div().min_w_0().truncate().type_title().text_color(theme::text()).child(network.label()))
                                .child(glyph("icons/chevron-down.svg", 12., theme::text_faint())),
                        );
                        strip.child(self.segments(&choices, cx, |this, ix, cx| this.set_home_unread(ix == 1, cx)))
                    }
                    _ => strip.child(pane_title(network.label())).child(
                        quiet(icon_button("network-menu", "icons/ellipsis.svg", false))
                            .tooltip(tip("Accounts", None))
                            .on_click(cx.listener(move |this, _, _, cx| this.open_menu(Menu::Network(network), cx))),
                    ),
                }
            }
            Mode::Space(key) => {
                let space = self.spaces.iter().find(|space| &space.key == key);
                let name = space.map_or_else(String::new, |space| space.name.clone());
                let menu_key = key.clone();
                let icon = space.and_then(|space| self.media_path(&space.key, space.icon.as_ref()));
                strip
                    .pl(px(10.))
                    .child(back("space-up", format!("Back to {}", key.network.label()), cx))
                    .child(Avatar::new(key, &name, 20.).shape(Shape::Place).image_at(icon).build())
                    .child(
                        div()
                            .id("space-menu")
                            .flex_1()
                            .min_w_0()
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| this.open_menu(Menu::Space(menu_key.clone()), cx)))
                            .child(div().min_w_0().truncate().type_title().text_color(theme::text()).child(name))
                            .child(glyph("icons/chevron-down.svg", 12., theme::text_faint())),
                    )
            }
        }
    }

    /// The Slack home's strip: the workspace is the title and its switcher (`02` §5).
    fn slack_title(&self, strip: Div, cx: &mut Context<Self>) -> Div {
        let Some(key) = self.slack_space() else {
            return strip.child(network_icon(Network::Slack, 20.)).child(pane_title("Slack"));
        };
        let space = self.spaces.iter().find(|s| s.key == key);
        let name = space.map_or_else(|| "Slack".to_owned(), |s| s.name.clone());
        let icon = space.and_then(|space| self.media_path(&space.key, space.icon.as_ref()));
        let others: Vec<&protocol::Space> = self.spaces.iter().filter(|s| s.key.network == Network::Slack && s.key != key).collect();
        // The filled pill is for mentions alone (C6); a Space's `attention` also counts its DMs.
        let mentions = self.slack_mentions(&others);
        let attention = others.iter().map(|s| s.attention).sum::<u32>();
        let elsewhere = match (mentions, attention, others.iter().any(|s| s.unread)) {
            (0, 0, false) => None,
            (0, 0, true) => Some(badge_dot(8., theme::unread())),
            (0, count, _) => Some(super::components::badge_count_unread(count)),
            (count, _, _) => Some(badge_mention(count)),
        };
        let unread = self.sidebar_state.home_unread;
        let news = self.trees.get(&key).map_or(0, |tree| tree.conversations.iter().filter(|c| c.badge() != Badge::None).count() as u32);
        let choices = [("slack-all", "All", 0, !unread), ("slack-unread", "Unread", news, unread)];
        strip
            .child(
                div()
                    .id("slack-workspace")
                    .flex_1()
                    .min_w_0()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_state.slack_menu = !this.sidebar_state.slack_menu;
                        cx.notify();
                    }))
                    .child(Avatar::new(&key, &name, 20.).shape(Shape::Place).image_at(icon).build())
                    .child(div().min_w_0().truncate().type_title().text_color(theme::text()).child(name))
                    .child(glyph("icons/chevron-down.svg", 12., theme::text_faint()))
                    .children(elsewhere),
            )
            .child(self.segments(&choices, cx, |this, ix, cx| this.set_home_unread(ix == 1, cx)))
    }

    /// The mentions of me in these workspaces, from the rows the shell holds: their trees where
    /// read, and the Inbox. Each conversation once.
    fn slack_mentions(&self, spaces: &[&protocol::Space]) -> u32 {
        let inside = |c: &&Rc<Conversation>| spaces.iter().any(|s| s.key.account == c.key.account && c.space.as_deref().is_none_or(|id| id == s.key.id));
        let trees = spaces.iter().filter_map(|s| self.trees.get(&s.key)).flat_map(|tree| tree.conversations.iter());
        let mut seen = HashSet::new();
        trees.chain(self.sidebar_state.inbox.all().iter()).filter(inside).filter(|c| seen.insert(c.key.clone())).map(|c| c.mentions).sum()
    }

    /// The Slack header's menu: the workspaces, then signing out of this one.
    // CONTRACT: status, pause notifications, away and "Open in Slack" have no request behind
    // them in the protocol, so the menu holds the workspaces and sign-out only.
    fn slack_menu(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.sidebar_state.slack_menu {
            return None;
        }
        let current = self.slack_space();
        let snapshot = self.snapshot.as_ref()?;
        let mut menu = super::components::menu().w(px(280.));
        let workspaces: Vec<&protocol::Space> = self.spaces.iter().filter(|s| s.key.network == Network::Slack).collect();
        if workspaces.len() > 1 {
            menu = menu.child(super::components::menu_label("Workspaces"));
            for (ix, space) in workspaces.iter().enumerate() {
                let key = space.key.clone();
                let link = snapshot.account_link(&space.key);
                let words = (link != Link::Connected).then(|| super::components::link_word(link));
                let row = div()
                    .id(("workspace", ix))
                    .h(px(28.))
                    .px(px(8.))
                    .mx(px(4.))
                    .rounded(px(theme::R_CONTROL))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .when(current.as_ref() == Some(&space.key), |row| row.bg(theme::selected()))
                    .hover(|style| style.bg(theme::hover()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sidebar_state.slack = Some(key.clone());
                        this.sidebar_state.slack_menu = false;
                        this.show_mode(cx);
                    }))
                    .child(Avatar::new(&space.key, &space.name, 20.).shape(Shape::Place).build())
                    .child(div().flex_1().min_w_0().truncate().type_ui().text_color(theme::text()).child(space.name.clone()))
                    .children(words.map(|words| div().type_small().text_color(theme::text_soft()).child(words)))
                    .children(rollup_badge(space.attention, space.unread))
                    .child(div().type_small().text_color(theme::text_soft()).child(crate::gui::shortcuts::label(&format!("⌃{}", ix + 1))));
                menu = menu.child(row);
            }
            menu = menu.child(super::components::menu_separator());
        }
        let account = current.as_ref().map(Key::account);
        let name = current.as_ref().and_then(|key| self.spaces.iter().find(|s| &s.key == key)).map_or_else(String::new, |s| s.name.clone());
        menu = menu.child(super::components::menu_item_danger("slack-sign-out", Some("icons/log-out.svg"), format!("Sign out of {name}")).on_click(cx.listener(move |this, _, _, cx| {
            this.sidebar_state.slack_menu = false;
            if let Some(account) = account.clone() {
                this.sign_out(account, cx);
            }
        })));
        Some(
            div()
                .absolute()
                .top(px(theme::STRIP - 6.))
                .left(px(12.))
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.sidebar_state.slack_menu = false;
                    cx.notify();
                }))
                .child(menu),
        )
    }

    fn set_home_unread(&mut self, unread: bool, cx: &mut Context<Self>) {
        self.sidebar_state.home_unread = unread;
        self.sidebar_state.cursor = None;
        self.rebuild_rows(cx);
    }

    /// The jump strip: a field that opens the switcher, and the compose button. People draws
    /// its own search here instead.
    fn jump_strip(&self, cx: &mut Context<Self>) -> Div {
        div()
            .h(px(44.))
            .flex_none()
            .px(px(theme::PANE_PAD))
            .pb(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .id("jump")
                    .flex_1()
                    .h(px(theme::CONTROL))
                    .px(px(8.))
                    .rounded(px(theme::R_CONTROL))
                    .bg(theme::field())
                    .border_1()
                    .border_color(theme::border())
                    .hover(|style| style.border_color(theme::border_strong()))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| this.open_switcher("", window, cx)))
                    .child(glyph("icons/search.svg", 14., theme::text_faint()))
                    .child(div().flex_1().type_ui().text_color(theme::text_faint()).child("Jump to…"))
                    .child(keycap("⌘K")),
            )
            .child(
                icon_button("compose", "icons/square-pen.svg", false)
                    .tooltip(tip("New message", Some("⌘N")))
                    .on_click(cx.listener(|this, _, window, cx| this.open_switcher("@", window, cx))),
            )
    }

    fn list(&self, snapshot: &Snapshot, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = &self.sidebar_state;
        let empty = |icon, title: &str, detail: &str| empty_state(icon, title, detail).px(px(24.)).into_any_element();
        match &state.mode {
            Mode::People => div().into_any_element(),
            Mode::Inbox => self.inbox_list(window, cx),
            // No rooms. People on this server are drawn in the window, not as channels.
            Mode::Network(Network::Tiny) => match snapshot.link(Network::Tiny) {
                Link::Connected => div()
                    .size_full()
                    .px(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .max_w(px(220.))
                            .text_center()
                            .whitespace_normal()
                            .type_ui()
                            .text_color(theme::text_soft())
                            .child("People on this server are in the window."),
                    )
                    .into_any_element(),
                Link::Offline => empty("icons/wifi-off.svg", "Tiny server is offline", "It shows up here once it connects."),
                _ => empty("icons/users.svg", "Tiny server", "Sign in to see who's here."),
            },
            _ if state.listed() => self.home_list(snapshot, window, cx),
            Mode::Network(network) if state.rows.is_empty() && state.spaces_loaded => {
                let name = network.label();
                match snapshot.link(*network) {
                    Link::Offline => empty("icons/wifi-off.svg", &format!("{name} is offline"), "What was saved shows up here once it connects."),
                    _ => empty("icons/hash.svg", "You're not in any channels yet", &format!("Channels you join in {name} show up here.")),
                }
            }
            Mode::Space(_) if state.rows.is_empty() => empty("icons/hash.svg", "No channels you can read", "Channels you are allowed to see show up here."),
            _ => uniform_list("tree", state.rows.len(), cx.processor(Self::tree_rows))
                .track_scroll(&state.tree_scroll)
                .size_full()
                .into_any_element(),
        }
    }

    /// The persistent Discord parent column, using the same home list and scroll state.
    pub(super) fn discord_overview(&self, snapshot: &Snapshot, width: f32, window: &mut Window, cx: &mut Context<Self>) -> Div {
        self.sidebar_state.width.set(width);
        div().w(px(width)).h_full().flex_none().bg(theme::sidebar()).border_r_1().border_color(theme::hairline()).child(
            div().id("discord-overview").size_full().flex().flex_col()
                .child(self.title_strip(&Mode::Network(Network::Discord), cx))
                .child(self.jump_strip(cx))
                .child(div().flex_1().min_h_0().child(self.home_list(snapshot, window, cx)))
        )
    }

    pub(super) fn sidebar(&self, snapshot: &Snapshot, width: f32, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let people = self.sidebar_state.mode == Mode::People;
        let tiny = matches!(self.sidebar_state.mode, Mode::Network(Network::Tiny));
        if !matches!(&self.sidebar_state.mode, Mode::Space(space) if space.network == Network::Discord) {
            self.sidebar_state.width.set(width);
        }
        let body = if people {
            let drawn = self.slots.people.and_then(|draw| draw(self, window, cx));
            drawn.unwrap_or_else(|| empty_state("icons/users.svg", "People", "Friends and contacts show up here.").px(px(24.)).into_any_element())
        } else {
            self.list(snapshot, window, cx)
        };
        let banners = self.banners(snapshot, cx);
        let tabs = self.folder_tabs(cx);
        div()
            .relative()
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme::sidebar())
            .border_r_1()
            .border_color(theme::hairline())
            .child(self.title_strip(&self.sidebar_state.mode, cx))
            .when(!people && !tiny, |sidebar| sidebar.child(self.jump_strip(cx)))
            .children(tabs)
            .when(!banners.is_empty(), |sidebar| sidebar.child(div().flex_none().px(px(theme::INSET)).pb(px(8.)).flex().flex_col().gap(px(4.)).children(banners)))
            .child(div().id("sidebar-list").flex_1().min_h_0().track_focus(&self.sidebar_state.focus).key_context("Sidebar").child(body))
            .children(super::playing::draw(self, window, cx))
            .children(self.slots.call_dock.and_then(|draw| draw(self, window, cx)))
            .children(self.slack_menu(cx))
    }
}

/// A tree row's name: a channel without its `#`; a Slack group DM by first names, "Mara,
/// Dev, +2", which would otherwise truncate to nothing.
fn tree_title(c: &Conversation) -> String {
    let title = c.title.trim_start_matches('#');
    if c.kind != ConversationKind::Group || c.key.network != Network::Slack {
        return title.to_owned();
    }
    let names: Vec<&str> = title.split(',').map(|name| name.split_whitespace().next().unwrap_or("")).filter(|name| !name.is_empty()).collect();
    match names.len() {
        0..=3 => names.join(", "),
        n => format!("{}, +{}", names[..2].join(", "), n - 2),
    }
}

/// How urgent an account's banner is, lowest first: signed out, then the network wanting
/// something done in its app, then a link that is reconnecting.
fn urgency(tone: Tone) -> u8 {
    match tone {
        Tone::Danger => 0,
        Tone::Attention => 1,
        Tone::Warning => 2,
        Tone::Info | Tone::Keychain => 3,
    }
}

/// Where the network's own app is, for what only it can do (verify an account, solve a captcha).
fn network_app(network: Network) -> &'static str {
    match network {
        Network::Discord => "https://discord.com/app",
        Network::Slack => "https://app.slack.com/client",
        Network::Telegram => "https://web.telegram.org/",
        Network::Teams => "https://teams.microsoft.com/",
        // Signal has no web app: what only it can do is done on the phone.
        Network::Signal => "https://signal.org/download/",
        Network::Messages => "https://messages.google.com/web/",
        // Nothing only a Tiny server's own app can do is asked from here.
        Network::Tiny => "",
    }
}

/// What a pane says about one account's link, and in which tone. `several`: its network has
/// more than one account, so the account is named. `mixed`: the pane lists every account (the
/// Inbox), where only what needs the user is said. `long` answers whether the link has been
/// down long enough to mention.
fn banner_words(network: Network, several: bool, status: &protocol::AccountStatus, mixed: bool, long: impl Fn() -> bool) -> Option<(Tone, String)> {
    let (name, account) = (network.label(), status.name.as_str());
    // The network's own words for what it wants done in its app.
    if !status.required.is_empty() {
        return Some((Tone::Attention, status.required.clone()));
    }
    match status.link {
        Link::Expired if several => Some((Tone::Danger, format!("{name} signed you out of {account}"))),
        Link::Expired => Some((Tone::Danger, format!("{name} signed you out"))),
        Link::Offline if long() => Some(match (status.reconnecting, several || mixed) {
            (true, false) => (Tone::Warning, "Reconnecting…".to_owned()),
            (true, true) => (Tone::Warning, format!("{name} is reconnecting")),
            (false, false) => (Tone::Warning, format!("Can't reach {name}")),
            (false, true) => (Tone::Warning, format!("Can't reach {name} · {account}")),
        }),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use protocol::{AccountKey, AccountStatus, Presence};

    pub fn row(id: &str, kind: ConversationKind) -> Conversation {
        Conversation::new(Key { network: Network::Discord, account: "me".into(), id: id.into() }, kind, id)
    }

    fn status(link: Link, reconnecting: bool, required: &str) -> AccountStatus {
        AccountStatus {
            key: AccountKey { network: Network::Slack, account: "T1".into() },
            name: "Fernhill".into(),
            link,
            detail: String::new(),
            me: String::new(),
            attention: 0,
            unread: false,
            reconnecting,
            required: required.into(),
            avatar: None,
            presence: Presence::Unknown,
            status: String::new(),
            can: Default::default(),
        }
    }

    #[test]
    fn a_pane_says_what_is_wrong_with_a_link_in_the_specs_words_and_nothing_when_healthy() {
        let say = |status: &AccountStatus, several, mixed, long| banner_words(Network::Slack, several, status, mixed, || long).map(|(_, words)| words);
        assert_eq!(say(&status(Link::Connected, false, ""), false, false, true), None);
        assert_eq!(say(&status(Link::Expired, false, ""), false, true, false).as_deref(), Some("Slack signed you out"));
        assert_eq!(say(&status(Link::Expired, false, ""), true, false, false).as_deref(), Some("Slack signed you out of Fernhill"));
        // The first seconds of any connect say nothing; then it depends on whether chatd is trying.
        assert_eq!(say(&status(Link::Offline, true, ""), false, false, false), None);
        assert_eq!(say(&status(Link::Offline, true, ""), false, false, true).as_deref(), Some("Reconnecting…"));
        assert_eq!(say(&status(Link::Offline, false, ""), false, false, true).as_deref(), Some("Can't reach Slack"));
        // The Inbox mixes networks, so it names the one that is late.
        assert_eq!(say(&status(Link::Offline, true, ""), false, true, true).as_deref(), Some("Slack is reconnecting"));
        let wanted = "Discord wants you to verify your account in its app.";
        assert_eq!(say(&status(Link::Connected, false, wanted), false, true, false).as_deref(), Some(wanted));
    }

    #[test]
    fn the_most_urgent_account_speaks_signed_out_then_needs_you_then_reconnecting() {
        let mut tones = vec![Tone::Warning, Tone::Attention, Tone::Danger];
        tones.sort_by_key(|tone| urgency(*tone));
        assert!(tones == [Tone::Danger, Tone::Attention, Tone::Warning]);
    }

    #[test]
    fn a_mixed_list_knows_which_rows_are_on_screen_from_its_heights() {
        let list = ListState::new(10, ListAlignment::Top, px(0.));
        // No layout yet: a window's worth from the top.
        assert_eq!(on_screen(&list, 10, |_| 48.), 0..10);
        assert_eq!(on_screen(&list, 40, |_| 48.), 0..19);
        list.scroll_to(gpui::ListOffset { item_ix: 5, offset_in_item: px(10.) });
        assert_eq!(on_screen(&list, 40, |_| 100.).start, 5);
    }
}
