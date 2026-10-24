//! The contract between the processes: the shell and chatd over the chat socket, chatd and mediad
//! over mediad's stdin and stdout. Length-prefixed JSON, local only, at most 1 MiB a frame.
//!
//! Identity. Everything belongs to an account, and an account is `(network, account)`: one Slack
//! sign-in yields one account per workspace. Conversations, Spaces and people are named by a
//! [`Key`], which adds the network's own id. A Space is a server, workspace or team.
//!
//! Every list request is bounded. Nothing answers with "everything".

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;

mod call;
pub mod crash;
mod info;
pub mod ipc;
pub mod launch;
mod message;
mod play;
mod react;
pub mod settings;
mod signin;
mod tiny;

pub use call::*;
pub use crash::redact;
pub use info::*;
pub use ipc::{ChatListener, ChatStream};
pub use message::*;
pub use play::*;
pub use react::*;
pub use signin::*;
pub use tiny::*;

pub const CHAT_SOCK_NAME: &str = "chat.sock";

/// Most rows one answer carries, whatever the request asked for.
pub const PAGE_LIMIT: u32 = 200;
/// Most people the shell may watch at once (presence).
pub const WATCH_LIMIT: usize = 500;
/// Most people a voice channel's row names. `Occupants::total` says how many there are.
pub const OCCUPANTS_LIMIT: usize = 25;
/// Most Spaces a [`Profile`] lists as shared.
pub const MUTUAL_SPACES: usize = 20;
/// Most labelled lines a [`Profile`] carries.
pub const PROFILE_FIELDS: usize = 12;
/// Fewest characters of a people search that reach a network.
pub const SEARCH_MIN_CHARS: usize = 2;
/// Most characters of a draft chatd keeps.
pub const DRAFT_LIMIT: usize = 4_000;

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn is_zero(value: &u32) -> bool {
    *value == 0
}

pub(crate) fn is_zero64(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Telegram,
    Discord,
    Slack,
    Teams,
    Signal,
    /// Android Messages: Google Messages on the phone, paired to this computer.
    Messages,
    /// A Tiny server the owner already runs. Optional, and separate from the others.
    Tiny,
}

impl Network {
    pub const ALL: [Network; 7] = [
        Network::Telegram,
        Network::Discord,
        Network::Slack,
        Network::Teams,
        Network::Signal,
        Network::Messages,
        Network::Tiny,
    ];

    /// The label the shell shows on a row. The network is a label, never a different layout.
    pub fn label(self) -> &'static str {
        match self {
            Network::Telegram => "Telegram",
            Network::Discord => "Discord",
            Network::Slack => "Slack",
            Network::Teams => "Teams",
            Network::Signal => "Signal",
            Network::Messages => "Messages",
            Network::Tiny => "Tiny server",
        }
    }
}

/// One signed-in identity. `account` is chosen by the adapter and is stable across restarts: a
/// Slack team id, a Discord user id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountKey {
    pub network: Network,
    pub account: String,
}

/// Names a conversation, a Space or a person: the account it belongs to and the network's own id.
/// Which of the three it is follows from the field it is in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key {
    pub network: Network,
    pub account: String,
    pub id: String,
}

impl Key {
    pub fn new(account: &AccountKey, id: impl Into<String>) -> Key {
        Key {
            network: account.network,
            account: account.account.clone(),
            id: id.into(),
        }
    }

    pub fn account(&self) -> AccountKey {
        AccountKey {
            network: self.network,
            account: self.account.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    /// No session. This is the default until the user signs in.
    SignedOut,
    /// The network ended the session. What is cached stays readable; sending is refused until the
    /// user signs in again.
    Expired,
    /// Signed in, but the socket is down or still connecting. `AccountStatus::reconnecting` says
    /// whether chatd is trying right now.
    Offline,
    Connected,
}

/// A network as a whole: the best link of its accounts, `SignedOut` when it has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub network: Network,
    pub link: Link,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountStatus {
    pub key: AccountKey,
    /// The workspace's name, or the user's own name where an account is a person.
    pub name: String,
    pub link: Link,
    /// Why the link is what it is, in words, when the network said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Our own person id on this account.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub me: String,
    /// The sum of the numbers on this account's rows (see [`Conversation::badge`]).
    pub attention: u32,
    /// Something unmuted is unread that has no number.
    pub unread: bool,
    /// With `Offline`: the connection dropped and chatd is trying again within seconds. False
    /// while `Offline` means it has not connected yet, or has backed off to trying rarely.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reconnecting: bool,
    /// Something only the network's own app can do stands in the way (a captcha, a
    /// verification), in words: "Discord wants you to verify your account in its app." Empty
    /// when nothing does. The link may be anything; `Connected` with this set still fails
    /// most requests.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub required: String,
    /// Our own picture on this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<MediaFile>,
    /// Our own presence and status text as the network shows them to others.
    #[serde(default)]
    pub presence: Presence,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// What this account may send: file limits, pickers and options. All off until the adapter
    /// reports them.
    #[serde(default, skip_serializing_if = "Capabilities::is_none")]
    pub can: Capabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Media {
    /// mediad is not running. Calls and screen share are impossible in this state.
    Stopped,
    Running,
}

/// A server, workspace or team. Every account also has its direct conversations, which belong to
/// no Space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    pub key: Key,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<MediaFile>,
    /// The network's own order.
    pub position: i32,
    pub attention: u32,
    pub unread: bool,
    /// The folder it is filed in (Discord's server folders), by `Folder::key` id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Muted as a whole: every row in it is drawn dimmed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
    /// The user's own "hide muted channels" for this Space: muted rows with nothing for me are
    /// not drawn in its tree.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_muted: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupKind {
    /// A category or sidebar section of channels.
    #[default]
    Section,
    /// The direct and group conversations of a Space whose network keeps them inside it (a
    /// Slack workspace). Its rows are `Direct` and `Group` conversations with `space` set.
    Directs,
}

impl GroupKind {
    fn is_section(&self) -> bool {
        *self == GroupKind::Section
    }
}

/// A collapsible header inside a Space: a category, a sidebar section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub position: i32,
    /// Remembered by chatd, never sent to the network.
    #[serde(default, skip_serializing_if = "is_false")]
    pub collapsed: bool,
    #[serde(default, skip_serializing_if = "GroupKind::is_section")]
    pub kind: GroupKind,
    /// How many conversations sit under it, whatever page of them an answer carries. Filled by
    /// chatd on `Tree`; an adapter leaves it 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub count: u32,
    /// Muted on the network (a muted category): it starts collapsed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
    /// The user's own "unread only" for this section (Slack): rows with nothing unread are not
    /// drawn, except the open one and starred ones.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unread_only: bool,
}

impl Group {
    pub fn new(id: impl Into<String>, name: impl Into<String>, position: i32) -> Group {
        Group {
            id: id.into(),
            name: name.into(),
            position,
            collapsed: false,
            kind: GroupKind::Section,
            count: 0,
            muted: false,
            unread_only: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Direct,
    /// A group direct message.
    Group,
    Channel,
    Announcement,
    Voice,
    Forum,
    Thread,
}

impl ConversationKind {
    /// Every unread message in these needs me; in the rest only mentions do.
    pub fn is_direct(self) -> bool {
        matches!(self, ConversationKind::Direct | ConversationKind::Group)
    }
}

/// What pressing the call button of a conversation does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallSupport {
    #[default]
    None,
    /// tinyclient carries the call itself.
    Native,
    /// The official app does: ask `Handoff` for the link and open it.
    Handoff,
}

/// Someone in a voice channel, as its row in the tree shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occupant {
    pub person: Key,
    pub name: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deafened: bool,
    /// Sharing a screen or a camera.
    #[serde(default, skip_serializing_if = "is_false")]
    pub streaming: bool,
}

/// Who is in a voice channel, whether or not we are. Never stored: it arrives as
/// `Change::Occupants` and rides on the row when the row is read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occupants {
    /// At most [`OCCUPANTS_LIMIT`], in the network's order. We are in it when we are in the
    /// channel.
    pub people: Vec<Occupant>,
    /// How many are in the channel in all.
    pub total: u32,
}

/// What a row shows beside its title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    None,
    Dot,
    Count(u32),
}

/// One row of the Inbox or of a Space's tree. No message bodies beyond the preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub key: Key,
    pub kind: ConversationKind,
    pub title: String,
    /// The Space it is in. Direct conversations have none, except on a network that keeps them
    /// inside a Space (see [`GroupKind::Directs`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// That Space's name, so a row can say where it is without a second request.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub space_title: String,
    /// The [`Group`] it sits under in its Space's tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// That Group's name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group_title: String,
    /// What the conversation is for, as its owners wrote it. One line, cut to 300 characters.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub topic: String,
    /// For a `Thread`: the conversation it hangs off, and the message there that started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// The network's own order inside its Space.
    #[serde(default)]
    pub position: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<MediaFile>,
    /// The other person of a `Direct` conversation, for presence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// Unread messages. For a channel the network may only say "some", which is 1.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unread: u32,
    /// Unread messages that mention me.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mentions: u32,
    /// Muted on the network. Drawn dimmed, never a dot, still a quiet mention count.
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
    /// Pinned to the top of the Inbox. Local; the network never hears of it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub starred: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preview: String,
    /// Unix seconds of the newest message.
    #[serde(default)]
    pub updated: i64,
    /// The newest message read, for the "New" divider. `Messages` answers with the value from
    /// before the shell marks anything read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read: Option<String>,
    #[serde(default)]
    pub call: CallSupport,
    /// Who is in it, for a voice channel someone is in. `None` when it is empty or unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupants: Option<Occupants>,
    /// I may read it but not post in it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    /// Only invited people see it: drawn with a lock.
    #[serde(default, skip_serializing_if = "is_false")]
    pub private: bool,
    /// What the user had typed and not sent, kept by chatd across restarts (`SetDraft`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub draft: String,

    // How the network files it. Shown only in that network's own home, never in the Inbox.
    /// The folders it is in (Telegram's chat folders), by `Folder::key` id. A chat in two
    /// folders is in both. At most [`FOLDERS_PER_ROW`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
    /// In the network's archive: out of the Inbox and every count unless it mentions me.
    #[serde(default, skip_serializing_if = "is_false")]
    pub archived: bool,
    /// Pinned on the network, and where among the pins (lowest first). Separate from `starred`,
    /// which is tinyclient's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<i32>,
    /// Hidden on the network (a Teams chat the user hid): never listed. The adapter unhides it
    /// when a new message arrives, as the network does.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// A followed thread (one the user joined or replied in): the Threads list holds these.
    #[serde(default, skip_serializing_if = "is_false")]
    pub followed: bool,

    // What it is.
    /// The other side is a bot or an app. Never among people: the Inbox folds these into one
    /// Apps row, and their plain unread raises no number anywhere.
    #[serde(default, skip_serializing_if = "is_false")]
    pub bot: bool,
    /// A meeting's chat (Teams). The shell moves one quiet for 14 days (`updated`) into a
    /// collapsed Meetings group.
    #[serde(default, skip_serializing_if = "is_false")]
    pub meeting: bool,
    #[serde(default, skip_serializing_if = "Special::is_none")]
    pub special: Special,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<Mark>,
    /// The user did not opt in to it (Discord's "show all channels" off): drawn only under the
    /// tree's "Browse n more channels".
    #[serde(default, skip_serializing_if = "is_false")]
    pub opted_out: bool,
    /// Filled by chatd on reading: nothing said in 30 days, nothing unread, not starred and no
    /// draft. Slack folds these into "n quiet channels".
    #[serde(default, skip_serializing_if = "is_false")]
    pub quiet: bool,

    // The preview line's parts.
    /// Who said the newest message, in a group or channel. Empty when it was me (`preview_mine`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preview_from: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub preview_mine: bool,
    /// My newest message has been read by the other side (Telegram's ✓✓, Teams' "Seen").
    #[serde(default, skip_serializing_if = "is_false")]
    pub seen: bool,
}

impl Conversation {
    /// A row with nothing said about it but what it is: for tests, and for the shell's own
    /// stand-in while the real row loads.
    pub fn new(key: Key, kind: ConversationKind, title: impl Into<String>) -> Conversation {
        Conversation {
            key,
            kind,
            title: title.into(),
            space: None,
            space_title: String::new(),
            group: None,
            group_title: String::new(),
            topic: String::new(),
            parent: None,
            root: None,
            position: 0,
            avatar: None,
            peer: None,
            unread: 0,
            mentions: 0,
            muted: false,
            starred: false,
            preview: String::new(),
            updated: 0,
            last_read: None,
            call: CallSupport::None,
            occupants: None,
            read_only: false,
            private: false,
            draft: String::new(),
            folders: Vec::new(),
            archived: false,
            pin: None,
            hidden: false,
            followed: false,
            bot: false,
            meeting: false,
            special: Special::None,
            mark: None,
            opted_out: false,
            quiet: false,
            preview_from: String::new(),
            preview_mine: false,
            seen: false,
        }
    }

    /// The line under the title in the header: `Space > group`, then the topic where known.
    /// Empty for a direct conversation outside any Space.
    pub fn subtitle(&self) -> String {
        let place = [self.space_title.as_str(), self.group_title.as_str()];
        let place: Vec<&str> = place.into_iter().filter(|part| !part.is_empty()).collect();
        let mut line = place.join(" › ");
        if !self.topic.is_empty() {
            if !line.is_empty() {
                line.push_str(" · ");
            }
            line.push_str(&self.topic);
        }
        line
    }

    /// The one rule for numbers and dots. A number means someone needs me: every unread message
    /// of a direct conversation with a person, each mention anywhere. A dot is an unmuted row
    /// with unread messages and no mention. A bot, an archived chat and a message request raise
    /// a number only for a mention.
    pub fn badge(&self) -> Badge {
        let quiet = self.bot || self.archived || self.special == Special::Request;
        let needs_me = if self.kind.is_direct() && !self.muted && !quiet {
            self.unread.max(self.mentions)
        } else {
            self.mentions
        };
        match (needs_me, self.unread, self.muted) {
            (0, 0, _) | (0, _, true) => Badge::None,
            (0, _, false) => Badge::Dot,
            (count, _, _) => Badge::Count(count),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// Seen somewhere: a message author, a search result.
    #[default]
    None,
    /// Someone I have a direct conversation with, on networks that have no friends.
    Contact,
    Friend,
    /// They asked to be my friend.
    Incoming,
    /// I asked.
    Outgoing,
    Blocked,
}

/// Never stored. Known only for people the shell is watching and for lists the user owns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// Not offline: drawn as no dot at all.
    #[default]
    Unknown,
    Online,
    Idle,
    Dnd,
    /// In a meeting, a call or a huddle (Teams' Busy): a solid red disc, distinct from
    /// do-not-disturb's bar.
    Busy,
    Offline,
}

impl Presence {
    /// Online, idle, do-not-disturb or busy: around.
    pub fn is_around(self) -> bool {
        matches!(self, Presence::Online | Presence::Idle | Presence::Dnd | Presence::Busy)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub key: Key,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<MediaFile>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bot: bool,
    #[serde(default)]
    pub relation: Relation,
    #[serde(default)]
    pub presence: Presence,
    /// Custom status text, as last seen.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Opens this person in the network's own app, where the network has such a link.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub link: String,
    /// Their status's emoji, apart from its words (`status`). Live, like presence.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub emoji: String,
    /// What they are doing now: playing, listening, in voice, in a meeting. Live. Boxed: few
    /// people in a list are doing anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<Box<Activity>>,
    /// Their job title (Slack, Teams). Stored.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Seconds east of UTC now, for "4:12 PM · 3 h behind you". Stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc_offset: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<Mark>,
}

impl Person {
    /// Someone known by name only.
    pub fn new(key: Key, name: impl Into<String>) -> Person {
        Person {
            key,
            name: name.into(),
            handle: String::new(),
            avatar: None,
            bot: false,
            relation: Relation::None,
            presence: Presence::Unknown,
            status: String::new(),
            link: String::new(),
            emoji: String::new(),
            activity: None,
            title: String::new(),
            utc_offset: None,
            mark: None,
        }
    }
}

/// What a person's card shows beyond the row: asked for one person at a time, never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub person: Person,
    /// Their own words about themselves, cut to 1,000 characters.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bio: String,
    /// The titles of the Spaces we share, at most [`MUTUAL_SPACES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mutual_spaces: Vec<String>,
    /// Their time zone as the network names it (`Europe/Lisbon`), and its offset from UTC in
    /// seconds now, so the shell can show their local time without a zone database.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub time_zone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc_offset: Option<i32>,
    /// What else the network says of them that has no typed place below, as label and value in
    /// its own words. At most [`PROFILE_FIELDS`]. Leftovers only: the shell draws them last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<(String, String)>,
    /// Unix seconds they joined the network ("Member since").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    /// Friends we have in common, where the network counts them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mutual_friends: u32,
    /// The colour of their profile's banner, `0xRRGGBB`, and its picture where they set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<MediaFile>,

    // Typed, in the card's fixed order. Each is drawn only when it has a value.
    /// Their nickname in the Space the card was opened from (`Profile.space`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nickname: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pronouns: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub department: String,
    /// Their roles in that Space, names only (colours are left out), at most [`PROFILE_ROLES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Email and phone, each copied on click; phone numbers masked until clicked. At most
    /// [`PROFILE_CONTACTS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<Contact>,
    /// How many Spaces we share in all, when that is more than `mutual_spaces` lists.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mutual_total: u32,
    /// Unix seconds: joined that Space, became friends, last seen online.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub friends_since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<i64>,
    /// When their status clears, unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_until: Option<i64>,
    /// Out of office, in the network's words, and until when.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub away: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub away_until: Option<i64>,
    /// The user's own private note on them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<Warning>,
}

impl Profile {
    /// A card that says nothing beyond the person's row.
    pub fn of(person: Person) -> Profile {
        Profile {
            person,
            bio: String::new(),
            mutual_spaces: Vec::new(),
            time_zone: String::new(),
            utc_offset: None,
            fields: Vec::new(),
            since: None,
            mutual_friends: 0,
            accent: None,
            banner: None,
            nickname: String::new(),
            pronouns: String::new(),
            title: String::new(),
            department: String::new(),
            roles: Vec::new(),
            contacts: Vec::new(),
            mutual_total: 0,
            joined: None,
            friends_since: None,
            last_seen: None,
            status_until: None,
            away: String::new(),
            away_until: None,
            note: String::new(),
            warning: None,
        }
    }
}

/// Which of the things said in a conversation `Shared` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedKind {
    /// Pictures, videos and GIFs.
    Media,
    /// Every other attachment: documents, archives, audio.
    Files,
    Links,
}

/// One thing shared in a conversation, and the message it was shared in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedItem {
    pub message_id: String,
    /// Unix seconds.
    pub sent: i64,
    pub sender: String,
    /// For `Media` and `Files`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<Attachment>,
    /// For `Links`: where it goes, and the page's title when the network made a preview.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

/// What the user does about a person. Never automated: only on a shell request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelateAction {
    /// Accept their friend request.
    Accept,
    /// Dismiss their friend request without telling them.
    Ignore,
    /// Withdraw the request I sent.
    Cancel,
    /// End the friendship.
    Remove,
    Block,
}

/// Someone typing in a conversation right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Typist {
    pub id: String,
    pub name: String,
    /// Unix seconds at which the line goes away if nothing more is heard. The shell times this
    /// out itself; chatd sends no change for it.
    pub until: i64,
}

/// One section of a member list, with how many people it holds on the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberSection {
    pub name: String,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The account's socket is down, so nothing was sent or stored.
    Offline,
    NotFound,
    CallActive,
    NoCall,
    Media,
    /// Sign-in was cancelled (the window was closed) or failed.
    SignIn,
    /// The network's sign-in page could not be loaded.
    SignInUnreachable,
    /// The user's organization does not allow this kind of sign-in.
    SignInBlocked,
    /// This network has no adapter, or its adapter does not do this yet.
    Unsupported,
    /// The network said no. `detail` is its reason, in words the user can read.
    Refused,
    /// The answer would not fit in one frame. Ask for a smaller page.
    TooLarge,
    Internal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxFilter {
    #[default]
    All,
    /// Anything unread or mentioning me that is not muted: what I have not seen.
    Unread,
    /// Only what mentions me, muted or not: who is waiting on me.
    Mentions,
    /// Direct and group conversations only.
    Directs,
}

/// How many rows each Inbox filter holds, for the same `network` as the request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxCounts {
    pub all: u32,
    pub unread: u32,
    pub mentions: u32,
    /// Bot and app rows with something unread: the Apps row's dot, never a number.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub apps: u32,
}

/// Which messages of a conversation. Always oldest first in the answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum Page {
    #[default]
    Newest,
    /// Older than this message. When the cache runs out chatd asks the network and reports the
    /// arrival as a `Messages` change.
    Before { id: String },
    After { id: String },
    /// Centred on this message, which is included.
    Around { id: String },
}

/// What the shell has on screen. Replaces the previous `Watch` as a whole. chatd asks the network
/// for presence, typing and members of exactly this and drops the rest. It lapses when the shell
/// stops asking `Wait`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watching {
    /// The open conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<Key>,
    /// People whose presence is drawn, at most [`WATCH_LIMIT`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub people: Vec<Key>,
    /// The rows of the open conversation's member list that are visible: first row and count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<(u32, u32)>,
    /// The Space whose tree is on screen, whether or not a conversation of it is open. An
    /// adapter keeps a Space's live subscription only while it is named here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<Key>,
    /// The conversation rows on screen in any list, at most [`ROWS_WATCHED`]. chatd fills in
    /// the preview of those that have none, paced, on every network but Discord.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Key>,
}

/// One hit of a message search: where the message is, and enough of it to choose by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHit {
    pub conversation: Key,
    /// The message's id, to land on it with `Page::Around`.
    pub message_id: String,
    /// Unix seconds.
    pub sent: i64,
    /// Who sent it, as the message's own row says.
    pub sender: String,
    /// The text around the match, on one line, at most 200 characters.
    pub snippet: String,
    /// The conversation's title, so a result row says where it is without a second request.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// The title of the Space the conversation sits in, when it sits in one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub space_title: String,
    /// Mine, so the row can say "You".
    #[serde(default, skip_serializing_if = "is_false")]
    pub outgoing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    /// Answers with `Changes` once anything has changed since `since`, or after a while with none.
    /// This is how the shell hears about everything without polling. Start from `Status.seq`.
    Wait {
        since: u64,
    },

    /// The conversations I am in that I would answer, starred first, then newest first; for
    /// `Unread` and `Mentions`, mentions first, then newest. In it: direct and group
    /// conversations, starred rows, rows the adapter lists, and channels that mention me (for
    /// 24 h after the mention, read or not). Never: hidden rows, message requests, rows with no
    /// message at all, archived rows that do not mention me, and anything quiet for 30 days
    /// that is not starred, unread, drafted or holding a failed send. Bot rows are in it with
    /// `bot` set: the shell folds them into one Apps row. Day groups are the shell's, from
    /// `updated`.
    Inbox {
        network: Option<Network>,
        #[serde(default)]
        filter: InboxFilter,
        offset: u32,
        limit: u32,
    },
    /// Every Space of a network, or of all of them, in the network's order. Small by nature.
    Spaces {
        network: Option<Network>,
    },
    /// The groups of one Space and a page of its conversations in tree order. Asking is what makes
    /// chatd bring a Space up to date; a Space nobody opened costs nothing.
    Tree {
        space: Key,
        offset: u32,
        limit: u32,
        /// Read what is stored and bring nothing up to date. The quick switcher reads many Spaces
        /// at once, and that must not subscribe to every one of them on the network.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        peek: bool,
    },
    /// One row, for after a `Conversation` change.
    Conversation {
        conversation: Key,
    },
    Collapse {
        space: Key,
        group: String,
        collapsed: bool,
    },
    Star {
        conversation: Key,
        starred: bool,
    },
    /// Conversations whose title contains `query`, across every Space, most recent first. From
    /// the cache alone: a channel of a Space never opened is not in it.
    Find {
        network: Option<Network>,
        query: String,
        limit: u32,
    },
    /// Cached messages whose text contains `query`, newest first. `conversation` narrows to one
    /// conversation, `network` to one network, `space` to one Space's conversations. Hidden
    /// conversations and message requests stay out of it, as they stay out of everything that
    /// counts. From the cache alone: what a network itself finds lands as it answers, and is a
    /// later step.
    SearchMessages {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<Key>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        network: Option<Network>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<Key>,
        query: String,
        limit: u32,
    },
    /// Ask the network itself for messages, as the pane's Enter does: a server's own search
    /// when a Space is the scope, the channel's when one conversation is. The hits are mapped
    /// like the cache's, so the pane can merge the two lists and tell them apart; a network
    /// that cannot search here answers why.
    SearchNetwork {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<Key>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<Key>,
        query: String,
    },
    /// Keeps what the user typed and did not send. Empty text forgets it. Local: no network
    /// hears of it and no change is announced.
    SetDraft {
        conversation: Key,
        text: String,
    },
    /// A network's own list of the conversations that belong to no Space (Discord's DMs, every
    /// Telegram chat, Teams' chats), as its home shows them: the network's pins in their order,
    /// then newest first. Hidden rows never; the archive and message requests only on their
    /// own `shelf`. With `folder`, the chats in that folder whatever their archive, the
    /// folder's own pins first. `counts` in the answer are this list's. Asking with `offset` 0
    /// is what makes chatd bring it up to date.
    Home {
        network: Network,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<AccountKey>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<String>,
        #[serde(default, skip_serializing_if = "Shelf::is_main")]
        shelf: Shelf,
        offset: u32,
        limit: u32,
    },
    /// The folders of a network's accounts in their order: Telegram's chat folders with their
    /// unread counts, Discord's server folders with their Spaces.
    Folders {
        network: Network,
    },
    /// The threads the user follows in a Space (a Slack workspace's Threads view, a Discord
    /// server's joined threads), newest first. Asking with `offset` 0 is what makes chatd fetch
    /// the network's own list, once; it is kept fresh by the socket after that.
    Threads {
        space: Key,
        offset: u32,
        limit: u32,
    },
    /// The topics of a forum (a Telegram forum group, a Discord forum), the network's pins
    /// first, then newest first. From the cache; opening a topic is opening a conversation.
    Topics {
        forum: Key,
        offset: u32,
        limit: u32,
    },
    /// Voice channels and calls with a friend in them now, across Spaces: the Discord home's
    /// "In voice". From what chatd holds live; at most 50.
    InVoice {
        network: Option<Network>,
    },
    /// Who could I talk to now: pending requests counted, then people in a call, online, and
    /// talked to in the last 14 days, each A to Z, at most [`AROUND_LIMIT`]. Bots never.
    Around {
        network: Option<Network>,
    },

    /// The lists the user owns (friends, contacts, direct partners), A to Z. With `query`, cached
    /// matches at once and the network's own search behind them as a `People` change. chatd
    /// decides when a network hears of it: a query of [`SEARCH_MIN_CHARS`] or more, the newest
    /// one only, once the typing has paused, to the connected accounts of `network`.
    People {
        network: Option<Network>,
        relation: Option<Relation>,
        query: Option<String>,
        offset: u32,
        limit: u32,
    },
    /// The visible rows of a conversation's member list. Never the whole list.
    Members {
        conversation: Key,
        offset: u32,
        limit: u32,
    },
    /// The direct conversation with a person, created if there is none. Nothing reaches the other
    /// person until a message is sent.
    StartDirect {
        person: Key,
    },
    /// One person's card, asked of the network when the card opens and kept five minutes.
    /// `space` is where it was opened from, for the nickname, roles and join date there.
    Profile {
        person: Key,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<Key>,
    },
    /// What the info pane says of a conversation beyond its row. Asked of the network when the
    /// pane opens and kept five minutes.
    ConversationInfo {
        conversation: Key,
    },
    /// A Space's own info, opened from its name.
    SpaceInfo {
        space: Key,
    },
    /// The network's notification setting for a conversation. The row changes when the network
    /// says so.
    SetMute {
        conversation: Key,
        notify: Notify,
    },
    /// Leaves a group conversation on the network. The shell asks once to confirm first.
    Leave {
        conversation: Key,
    },
    /// Closes a direct conversation, as the network's own ×: it leaves the list until a new
    /// message brings it back. Nothing is deleted.
    CloseDirect {
        conversation: Key,
    },
    /// The messages pinned in a conversation, asked of the network, newest first, at most
    /// [`PINNED_LIMIT`].
    Pinned {
        conversation: Key,
    },
    /// The pictures, files or links said in a conversation, newest first, from what is cached:
    /// what the info pane lists under a person or a channel. `before` continues below a
    /// message already listed. With `search`, the network is asked instead, because the user
    /// clicked "Search all"; never otherwise.
    Shared {
        conversation: Key,
        kind: SharedKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        limit: u32,
        #[serde(default, skip_serializing_if = "is_false")]
        search: bool,
    },
    /// Answer, withdraw or end a friendship, or block. The list changes when the network says so
    /// (a `People` change).
    Relate {
        person: Key,
        action: RelateAction,
    },
    /// Our own presence on one account, and the status text beside it: `None` leaves the text
    /// as it is, empty clears it.
    SetPresence {
        account: AccountKey,
        presence: Presence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    Watch(Watching),

    /// A page of one conversation, oldest first. Reading is not marking read.
    Messages {
        conversation: Key,
        #[serde(default)]
        page: Page,
        limit: u32,
    },
    /// The user saw the conversation up to this message: the window is active, the conversation is
    /// open and the message is on screen. Read here and on the network.
    MarkRead {
        conversation: Key,
        message_id: String,
    },
    /// Answers at once with the message as `Pending`. `client_id` is made by the shell and comes
    /// back on the message; when the network accepts it the same row turns `Sent` under its real
    /// id, or `Failed` with the reason. Refused outright when the account is not connected.
    ///
    /// `with` carries files (streamed by chatd from their paths, with `Change::Upload` as they
    /// go) and options; the pending message lists the files at once. Refused when it asks
    /// for more than the account's `Capabilities` allow.
    Send {
        conversation: Key,
        client_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        #[serde(default, skip_serializing_if = "Outgoing::is_plain")]
        with: Outgoing,
    },
    /// Stops a send whose files are still uploading. It ends `Failed` ("Cancelled"), so Retry
    /// and Discard work on it as on any other.
    CancelSend {
        conversation: Key,
        client_id: String,
    },
    /// Sends a `Failed` message again.
    Retry {
        conversation: Key,
        client_id: String,
    },
    /// Removes a `Failed` or `Pending` message that never left.
    Discard {
        conversation: Key,
        client_id: String,
    },
    /// The user's draft is changing. chatd decides how often the network hears of it.
    Typing {
        conversation: Key,
    },
    /// Adds (`on`) or takes back our reaction. `emoji` is the emoji itself, or a custom one's
    /// `:name:` as `Reaction::emoji` spells it. Shown at once; what the network then says is
    /// what stays.
    React {
        conversation: Key,
        message_id: String,
        emoji: String,
        on: bool,
    },
    /// What reactions the conversation takes from this account: asked when it opens, kept
    /// five minutes. chatd refuses a `React` these rules refuse, before the network hears of it.
    Reactions {
        conversation: Key,
    },
    /// Replaces the text of one of our own messages. Nothing changes on screen until the
    /// network accepts it; a refusal is the answer.
    Edit {
        conversation: Key,
        message_id: String,
        text: String,
    },
    /// Deletes a message for everyone. Waits for the network, like `Edit`.
    Delete {
        conversation: Key,
        message_id: String,
    },

    /// Custom emoji, stickers or GIFs for the picker, asked when its tab opens and never before,
    /// kept five minutes. `space` narrows custom emoji to one Space's; `query` searches;
    /// `cursor` continues a page. At most [`CATALOG_LIMIT`] items.
    Catalog {
        account: AccountKey,
        kind: CatalogKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<Key>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
    },

    /// Asks for these files to be on disk. Answers with what is known now; each file that lands
    /// later is a `Media` change. Ask only for what is on screen.
    Fetch {
        account: AccountKey,
        ids: Vec<String>,
        /// The longest side of the whole picture at the scale it is drawn, in device pixels
        /// (points times the screen's scale; for a cropped tile, the uncropped picture). Where
        /// the adapter registered size variants, chatd fetches the smallest one at least this
        /// large, and fetches again when the file on disk is smaller. Such a file answers
        /// with no `path` until the larger one lands.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u32>,
        /// The user asked again for files that failed. Without this a failed file is answered
        /// with its `failed` reason and nothing is fetched.
        #[serde(default, skip_serializing_if = "is_false")]
        retry: bool,
    },

    /// Opens the network's sign-in and keeps the session. Answers when the user is done.
    SignIn {
        network: Network,
        /// The main window's top left corner in screen points, so the sign-in window opens
        /// over it and not on another display.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<(i32, i32)>,
    },
    /// Reads again the sessions the keychain refused (`Keychain::denied`): the user's "Try
    /// again". Answers `Status` at once; the read goes on behind it.
    ReadSessions,
    /// Forgets the account's session and everything stored for it. Waits up to 5 s for the
    /// network to end the session first.
    SignOut {
        account: AccountKey,
    },
    /// Begins, or reads again, a sign-in the shell draws itself (`signin.rs`). Answers with the
    /// step to draw. For a network that signs in through a web page this is `Unsupported`.
    SignInBegin {
        network: Network,
    },
    /// What the user did about the step on show. Answers with the step to draw next.
    SignInAnswer {
        network: Network,
        input: SignInInput,
    },
    /// The user closed the sheet. Nothing is kept.
    SignInCancel {
        network: Network,
    },

    /// Joins or rings. Starts mediad. One call at a time. Refused with `Unsupported`, and no
    /// mediad, unless the conversation's `call` is `Native`.
    CallStart {
        conversation: Key,
    },
    /// Answers the incoming ring. Starts mediad.
    CallAccept,
    /// Stops the incoming ring for both sides. mediad never starts.
    CallDecline,
    /// Leaves. Stops mediad.
    CallEnd,
    CallSet {
        muted: bool,
        deafened: bool,
    },
    /// Where the official app takes this conversation's call.
    Handoff {
        conversation: Key,
    },
    Media(MediaRequest),
    /// A voice message or audio file: play, pause, seek, speed, stop. Answers `Ok`; the rest is
    /// `Change::Playback`.
    Play(PlayRequest),

    /// Every stored setting (`settings`). Missing ones are at their defaults.
    Settings,
    /// Stores one setting, or with `None` returns it to its default. Answered `Ok`, then
    /// `Change::Settings`. Refused unless `settings::valid` (or, for `None`, `settings::known`).
    Set {
        scope: String,
        key: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// Reset all settings: every stored row goes, so every setting is at its default.
    ResetSettings,
    /// Stops chatd: ⌘Q, and closing the window while Keep receiving is off. Answered `Ok`
    /// first; a call is left on the network before the process exits.
    Shutdown,
    /// What tinyclient keeps on this Mac, in bytes (Settings › Storage).
    Storage,
    /// Deletes every cached message and file. Sessions, drafts and settings stay.
    Clear,
    /// How much a retention of `days` would delete now.
    Prunable {
        days: u32,
    },
    /// The microphone test: on, the level arrives as `Change::Meter` ten times a second until
    /// off, 30 s, or a call starts. In a call it is the call's own meter.
    TestMicrophone {
        on: bool,
    },
    /// Plays the test sound on the call speaker (V2): in the call's mediad, or one started for it.
    TestSound,
    /// Who is on this Tiny server. An owner or an admin also gets the invites and the audit.
    TinyCommunity {
        account: AccountKey,
    },
    /// Creates an invite for `role` (`admin` or `member`). The token is in the answer, once.
    TinyInvite {
        account: AccountKey,
        role: String,
    },
    /// Changes one member's role (`owner`, `admin`, or `member`).
    TinyRole {
        account: AccountKey,
        user_id: String,
        role: String,
    },
    /// Removes one member.
    TinyRemove {
        account: AccountKey,
        user_id: String,
    },
}

/// One thing that changed. The shell asks again for exactly that; the ephemeral kinds carry their
/// whole new value and never cause a list to be read again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum Change {
    /// Links, accounts or their totals: ask `Status`.
    Status,
    /// A row's title, preview, order or unread state: ask `Conversation`, or the list it is in.
    Conversation { conversation: Key },
    /// Messages arrived, were edited, deleted, reacted to or changed send state: ask `Messages`.
    Messages { conversation: Key },
    /// Spaces were added, removed or reordered: ask `Spaces`.
    Spaces { account: AccountKey },
    /// A Space's groups or conversations: ask `Tree`.
    Tree { space: Key },
    /// The owned lists or a search result: ask `People`.
    People { account: AccountKey },
    /// Rows of a conversation's member list were added, removed or moved: ask `Members` again
    /// for the rows on screen.
    Members { conversation: Key },
    /// A requested file is on disk (`path`), or could not be fetched (`failed` says why).
    Media { account: AccountKey, file: MediaFile },
    /// Ephemeral. Who is in a voice channel now; no people and a total of 0 when it emptied.
    Occupants { conversation: Key, occupants: Occupants },
    /// Ephemeral. A network finished the search for `query`, whether or not it found anyone.
    Searched { account: AccountKey, query: String },
    /// Ephemeral. Everyone typing in the conversation now; empty when they stopped.
    Typing { conversation: Key, typists: Vec<Typist> },
    /// A network's folders, or their counts: ask `Folders`.
    Folders { account: AccountKey },
    /// Ephemeral. Presence, status and what they are doing, all of it.
    Presence {
        person: Key,
        presence: Presence,
        status: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        emoji: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activity: Option<Activity>,
    },
    /// Ephemeral. How far the files of a pending message have gone, in bytes, at most four
    /// times a second. Its outcome is the message's own change.
    Upload { conversation: Key, client_id: String, sent: u64, total: u64 },
    /// Ephemeral. The call as it is now: who is speaking, mute, state. `None` when it is over.
    Call { call: Option<Call> },
    /// Ephemeral. A sign-in the shell is drawing moved on by itself: the code was scanned, or
    /// ran out and was replaced.
    SignIn { network: Network, step: SignInStep },
    /// Ephemeral. The voice message or audio file playing, paused or just ended; `None` once
    /// stopped.
    Playback { playback: Option<Playback> },
    /// A setting was stored or reset: ask `Settings`.
    Settings,
    /// Ephemeral. The microphone test's level, 0 to 100; `None` once the test is over.
    Meter { level: Option<u8> },
}

impl Change {
    /// Two changes to the same thing: the later one replaces the earlier.
    pub fn same_scope(&self, other: &Change) -> bool {
        match (self, other) {
            (Change::Status, Change::Status)
            | (Change::Call { .. }, Change::Call { .. })
            | (Change::Playback { .. }, Change::Playback { .. })
            | (Change::Settings, Change::Settings)
            | (Change::Meter { .. }, Change::Meter { .. }) => true,
            (Change::SignIn { network: a, .. }, Change::SignIn { network: b, .. }) => a == b,
            (Change::Conversation { conversation: a }, Change::Conversation { conversation: b })
            | (Change::Messages { conversation: a }, Change::Messages { conversation: b })
            | (Change::Typing { conversation: a, .. }, Change::Typing { conversation: b, .. })
            | (Change::Members { conversation: a }, Change::Members { conversation: b })
            | (Change::Occupants { conversation: a, .. }, Change::Occupants { conversation: b, .. })
            | (Change::Tree { space: a }, Change::Tree { space: b })
            | (Change::Presence { person: a, .. }, Change::Presence { person: b, .. }) => a == b,
            (Change::Upload { conversation: a, client_id: x, .. }, Change::Upload { conversation: b, client_id: y, .. }) => {
                a == b && x == y
            }
            (Change::Spaces { account: a }, Change::Spaces { account: b })
            | (Change::Folders { account: a }, Change::Folders { account: b })
            | (Change::Searched { account: a, .. }, Change::Searched { account: b, .. })
            | (Change::People { account: a }, Change::People { account: b }) => a == b,
            (Change::Media { account: a, file: x }, Change::Media { account: b, file: y }) => {
                a == b && x.id == y.id
            }
            _ => false,
        }
    }

    /// Typing, presence, occupants, a finished search and the call: the change is the whole
    /// new value, so nothing needs reading again.
    pub fn is_ephemeral(&self) -> bool {
        matches!(
            self,
            Change::Typing { .. }
                | Change::Presence { .. }
                | Change::Call { .. }
                | Change::Occupants { .. }
                | Change::Searched { .. }
                | Change::SignIn { .. }
                | Change::Upload { .. }
                | Change::Playback { .. }
                | Change::Meter { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Response {
    Pong,
    /// Done, with nothing to say.
    Ok,
    Status {
        networks: Vec<NetworkStatus>,
        accounts: Vec<AccountStatus>,
        media: Media,
        call: Option<Call>,
        /// Where `Wait` starts from.
        seq: u64,
        /// What chatd is doing with the keychain: reading, waiting on macOS, refused.
        #[serde(default, skip_serializing_if = "Keychain::is_settled")]
        keychain: Keychain,
        /// When this chatd started, unix milliseconds. A new value means it restarted: read
        /// everything on screen again.
        #[serde(default)]
        started: u64,
    },
    Changes {
        /// Pass this to the next `Wait`.
        seq: u64,
        /// chatd no longer knows what changed since `since` (it restarted, or the shell fell too
        /// far behind): read everything on screen again.
        reset: bool,
        changes: Vec<Change>,
    },
    Conversations {
        conversations: Vec<Conversation>,
        /// How many rows the request matches in all.
        total: u32,
        /// For `Inbox`: what each filter would hold. Zero for `Find`.
        #[serde(default)]
        counts: InboxCounts,
    },
    Conversation {
        conversation: Conversation,
    },
    Spaces {
        spaces: Vec<Space>,
    },
    Tree {
        space: Key,
        groups: Vec<Group>,
        conversations: Vec<Conversation>,
        total: u32,
    },
    People {
        people: Vec<Person>,
        total: u32,
    },
    Members {
        /// Discord cache miss: rows will arrive through the active viewport subscription.
        #[serde(default, skip_serializing_if = "is_false")]
        pending: bool,
        conversation: Key,
        sections: Vec<MemberSection>,
        offset: u32,
        members: Vec<Person>,
    },
    /// What is known now about what the shell watches.
    Watching {
        people: Vec<Person>,
        typists: Vec<Typist>,
    },
    Messages {
        conversation: Key,
        messages: Vec<Message>,
        /// Where the user had read to when asked.
        last_read: Option<String>,
        /// The cache holds messages older than the first one here.
        older: bool,
        /// The cache holds messages newer than the last one here.
        newer: bool,
    },
    /// Answers `SearchMessages`: the newest hits first, at most the limit asked for.
    MessageHits {
        hits: Vec<MessageHit>,
    },
    Sent {
        message: Message,
    },
    Media {
        files: Vec<MediaFile>,
    },
    Link {
        url: String,
    },
    Profile {
        profile: Box<Profile>,
    },
    Shared {
        conversation: Key,
        items: Vec<SharedItem>,
        /// The cache holds older messages that were not looked at yet, or the network has more.
        more: bool,
        /// The list is the network's own and whole (a search was asked, or the network keeps the
        /// list). False: it is only what is in the messages loaded here, and must say so.
        #[serde(default, skip_serializing_if = "is_false")]
        complete: bool,
        /// The exact total, only where the network counted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<u32>,
    },
    Folders {
        folders: Vec<Folder>,
    },
    Around {
        /// Incoming requests waiting on me (Discord's Pending), counted only.
        requests: u32,
        in_call: Vec<Person>,
        online: Vec<Person>,
        recent: Vec<Person>,
    },
    /// Boxed, as `Profile` is: few answers are one, and every answer would carry its size.
    ConversationInfo {
        info: Box<ConversationInfo>,
    },
    SpaceInfo {
        info: Box<SpaceInfo>,
    },
    Pinned {
        conversation: Key,
        messages: Vec<Message>,
    },
    Reactions {
        conversation: Key,
        rules: ReactionRules,
    },
    Catalog {
        items: Vec<CatalogItem>,
        /// Pass back for the next page; `None` at the end.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
    },
    /// Where a sign-in the shell draws stands.
    SignIn {
        network: Network,
        step: SignInStep,
    },
    Error {
        code: ErrorCode,
        detail: String,
    },
    Settings {
        values: Vec<settings::Setting>,
    },
    /// Bytes on this Mac: the message database and the cached images and files.
    Storage {
        messages: u64,
        files: u64,
    },
    /// What a shorter retention would delete: messages, and about how many bytes of them.
    Prunable {
        messages: u64,
        bytes: u64,
    },
    /// Who is on this Tiny server, plus the invites and the audit when this account may see them.
    TinyCommunity {
        community: TinyCommunity,
    },
    /// An invite that was just created. `code.token` is not kept.
    TinyCode {
        code: TinyCode,
    },
}

/// `TINYCLIENT_DIR` moves the socket and database, for tests and the demo seed.
pub fn support_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TINYCLIENT_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
        return home.join("Library/Application Support/tinyclient");
    }
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("tinyclient");
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(profile).join("AppData/Roaming/tinyclient");
        }
        return PathBuf::from(r"C:\ProgramData\tinyclient");
    }
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(data_home).join("tinyclient");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local/share/tinyclient")
}

pub fn chat_sock_path() -> PathBuf {
    support_dir().join(CHAT_SOCK_NAME)
}

/// The largest frame either side writes or reads.
pub const FRAME_LIMIT: usize = 1024 * 1024;

/// Writes one frame. A value larger than [`FRAME_LIMIT`] is refused with `InvalidData` and
/// nothing is written, so the other side never reads a frame it would reject.
pub fn write_frame(mut w: impl Write, value: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    if bytes.len() > FRAME_LIMIT {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame larger than 1 MiB"));
    }
    let len = u32::try_from(bytes.len()).map_err(std::io::Error::other)?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// One frame's bytes, unparsed: for a reader that answers a frame it cannot parse.
pub fn read_frame_bytes(mut r: impl Read) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > FRAME_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame larger than 1 MiB",
        ));
    }
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(r: impl Read) -> std::io::Result<T> {
    let bytes = read_frame_bytes(r)?;
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

/// mediad exists only while a call or share is up.
pub fn media_process_needed(active_calls: usize) -> bool {
    active_calls > 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn members_pending_is_explicit_and_old_complete_responses_still_decode() {
        let mut value = serde_json::json!({"op": "members", "conversation": {
            "network": "discord", "account": "account", "id": "channel"
        }, "offset": 0, "sections": [], "members": []});
        let complete: super::Response = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(complete, super::Response::Members { pending: false, .. }));
        assert!(serde_json::to_value(complete).unwrap().get("pending").is_none());
        value["pending"] = serde_json::json!(true);
        let pending: super::Response = serde_json::from_value(value).unwrap();
        assert!(matches!(pending, super::Response::Members { pending: true, .. }));
        assert_eq!(serde_json::to_value(pending).unwrap()["pending"], true);
    }

    use super::*;

    fn row(kind: ConversationKind, unread: u32, mentions: u32, muted: bool) -> Conversation {
        let key = Key {
            network: Network::Slack,
            account: "T1".into(),
            id: "C1".into(),
        };
        Conversation {
            unread,
            mentions,
            muted,
            ..Conversation::new(key, kind, "#platform")
        }
    }

    #[test]
    fn idle_does_not_need_mediad() {
        assert!(!media_process_needed(0));
        assert!(media_process_needed(1));
    }

    #[test]
    fn a_number_means_someone_needs_me_and_a_dot_means_only_unread() {
        use ConversationKind::{Channel, Direct};
        assert_eq!(row(Direct, 3, 0, false).badge(), Badge::Count(3));
        assert_eq!(row(Channel, 3, 0, false).badge(), Badge::Dot);
        assert_eq!(row(Channel, 3, 2, false).badge(), Badge::Count(2));
        assert_eq!(row(Channel, 0, 0, false).badge(), Badge::None);
        // Muted: no dot, no number for plain unread, but a mention still counts.
        assert_eq!(row(Channel, 3, 0, true).badge(), Badge::None);
        assert_eq!(row(Direct, 3, 0, true).badge(), Badge::None);
        assert_eq!(row(Channel, 3, 1, true).badge(), Badge::Count(1));
        // A bot, an archived chat and a request raise a number only for a mention.
        let app = Conversation { bot: true, ..row(Direct, 4, 0, false) };
        assert_eq!(app.badge(), Badge::Dot);
        assert_eq!(Conversation { mentions: 1, ..app }.badge(), Badge::Count(1));
        let archived = Conversation { archived: true, ..row(Direct, 4, 0, false) };
        assert_eq!(archived.badge(), Badge::Dot);
    }

    #[test]
    fn a_plain_send_and_a_status_from_before_the_polish_round_read_the_same() {
        let send: Request = serde_json::from_str(
            r#"{"op":"send","conversation":{"network":"slack","account":"T1","id":"C1"},"client_id":"1","text":"hi"}"#,
        )
        .unwrap();
        let Request::Send { with, .. } = &send else { panic!("not a send") };
        assert!(with.is_plain() && with.ping, "a reply pings unless asked not to");
        assert!(!serde_json::to_string(&send).unwrap().contains("with"));
        let status: Response = serde_json::from_str(r#"{"op":"status","networks":[],"accounts":[],"media":"stopped","call":null,"seq":3}"#).unwrap();
        assert!(matches!(status, Response::Status { started: 0, ref keychain, .. } if keychain.is_settled()));
        let quiet = Outgoing { ping: false, ..Outgoing::default() };
        let json = serde_json::to_string(&quiet).unwrap();
        assert_eq!(json, r#"{"ping":false}"#);
        assert_eq!(serde_json::from_str::<Outgoing>(&json).unwrap(), quiet);
    }

    #[test]
    fn a_message_search_and_its_hits_read_off_the_wire() {
        let ask: Request = serde_json::from_str(r#"{"op":"search_messages","query":"lichen","limit":20}"#).unwrap();
        let Request::SearchMessages { conversation, network, space, query, limit } = &ask else {
            panic!("not a search")
        };
        assert!(conversation.is_none() && network.is_none() && space.is_none() && query == "lichen" && *limit == 20);
        let with_filters = Request::SearchMessages {
            conversation: Some(Key { network: Network::Slack, account: "T1".into(), id: "C1".into() }),
            network: Some(Network::Slack),
            space: Some(Key { network: Network::Slack, account: "T1".into(), id: "S1".into() }),
            query: "lichen".into(),
            limit: 20,
        };
        assert_eq!(
            serde_json::to_string(&with_filters).unwrap(),
            r#"{"op":"search_messages","conversation":{"network":"slack","account":"T1","id":"C1"},"network":"slack","space":{"network":"slack","account":"T1","id":"S1"},"query":"lichen","limit":20}"#
        );
        let hit = MessageHit {
            conversation: Key { network: Network::Discord, account: "1".into(), id: "C2".into() },
            message_id: "9".into(),
            sent: 20,
            sender: "mara".into(),
            snippet: "hello world".into(),
            title: String::new(),
            space_title: String::new(),
            outgoing: false,
        };
        assert_eq!(
            serde_json::to_string(&Response::MessageHits { hits: vec![hit] }).unwrap(),
            r#"{"op":"message_hits","hits":[{"conversation":{"network":"discord","account":"1","id":"C2"},"message_id":"9","sent":20,"sender":"mara","snippet":"hello world"}]}"#
        );
        // The network's own ask: the scope is a conversation or a Space, the words ride along.
        let ask: Request = serde_json::from_str(r#"{"op":"search_network","query":"lichen"}"#).unwrap();
        let Request::SearchNetwork { conversation, space, query } = &ask else {
            panic!("not a network search")
        };
        assert!(conversation.is_none() && space.is_none() && query == "lichen");
        let asked = Request::SearchNetwork {
            conversation: Some(Key { network: Network::Discord, account: "1".into(), id: "C2".into() }),
            space: None,
            query: "lichen".into(),
        };
        assert_eq!(
            serde_json::to_string(&asked).unwrap(),
            r#"{"op":"search_network","conversation":{"network":"discord","account":"1","id":"C2"},"query":"lichen"}"#
        );
    }

    #[test]
    fn a_frame_too_large_to_read_is_never_written() {
        let mut out = Vec::new();
        let huge = "x".repeat(FRAME_LIMIT + 1);
        let err = write_frame(&mut out, &huge).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(out.is_empty());
    }

    #[test]
    fn a_later_change_replaces_an_earlier_one_to_the_same_thing_only() {
        let key = |id: &str| Key {
            network: Network::Discord,
            account: "1".into(),
            id: id.into(),
        };
        let typing = |id: &str, names: &[&str]| Change::Typing {
            conversation: key(id),
            typists: names
                .iter()
                .map(|name| Typist { id: (*name).into(), name: (*name).into(), until: 0 })
                .collect(),
        };
        assert!(typing("a", &["ana"]).same_scope(&typing("a", &[])));
        assert!(!typing("a", &["ana"]).same_scope(&typing("b", &["ana"])));
        assert!(!typing("a", &[]).same_scope(&Change::Messages { conversation: key("a") }));
        assert!(!Change::Messages { conversation: key("a") }
            .same_scope(&Change::Conversation { conversation: key("a") }));
    }

    #[test]
    fn frames_written_before_these_fields_existed_still_read() {
        let request: Request = serde_json::from_str(r#"{"op":"fetch","account":{"network":"slack","account":"T1"},"ids":["a"]}"#).unwrap();
        assert!(matches!(request, Request::Fetch { size: None, retry: false, .. }));
        let request: Request = serde_json::from_str(r#"{"op":"sign_in","network":"slack"}"#).unwrap();
        assert!(matches!(request, Request::SignIn { at: None, .. }));
        let thread: ThreadSummary = serde_json::from_str(r#"{"id":"t","replies":2,"last":5}"#).unwrap();
        assert_eq!((thread.participants.len(), thread.unread), (0, 0));
        let call = r#"{"conversation":{"network":"discord","account":"1","id":"c"},"state":"joined",
            "participants":[],"muted":false,"deafened":false,"joined":1,"engine_linked":true,"video":[]}"#;
        let call: Call = serde_json::from_str(call).unwrap();
        assert_eq!((call.speaking, call.level, call.inputs.len()), (false, None, 0));
    }

    #[test]
    fn a_message_is_cut_to_its_limits_and_the_header_line_names_the_place() {
        let mut message = Message::plain("1", "bot", "", 1, false);
        message.card = Some(Box::new(Card {
            title: "é".repeat(300),
            fields: (0..40).map(|n| (n.to_string(), "v".repeat(2_000))).collect(),
            ..Card::default()
        }));
        message.bound();
        let card = message.card.as_ref().unwrap();
        assert_eq!((card.title.chars().count(), card.fields.len(), card.fields[0].1.len()), (256, CARD_FIELDS, 1_000));

        let mut place = row(ConversationKind::Channel, 0, 0, false);
        assert_eq!(place.subtitle(), "");
        place.space_title = "Moss & Lichen".into();
        place.group_title = "Trail".into();
        assert_eq!(place.subtitle(), "Moss & Lichen › Trail");
        place.topic = "Where we walk".into();
        assert_eq!(place.subtitle(), "Moss & Lichen › Trail · Where we walk");
    }

    #[test]
    fn empty_fields_stay_off_the_wire() {
        let message = Message::plain("1", "mara", "hi", 1_700_000_000, false);
        let json = serde_json::to_string(&message).unwrap();
        for absent in ["spans", "attachments", "reactions", "reply_to", "thread", "client_id", "card", "forwarded", "deleted"] {
            assert!(!json.contains(absent), "{absent} in {json}");
        }
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
    }
}
