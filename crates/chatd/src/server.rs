use crate::adapters::{self, About, Adapter, Command, Refusal, Reply, SearchScope};
use crate::blobs::Blobs;
use crate::hub::{lock, now, Hub, KEYCHAIN_PROMPT};
use crate::media::CallError;
use crate::session;
use crate::store::Store;
use protocol::settings;
use protocol::{
    clip, read_frame_bytes, support_dir, write_frame, CatalogKind, ConversationInfo, MediaFile, MediaKind, Outgoing, SharedKind, SpaceInfo, AccountKey, AccountStatus, CallSupport, Capabilities,
    Change, ChatListener, ChatStream, Conversation, ConversationKind, ErrorCode, InboxCounts, Key,
    Link, Media, MediaRequest, Message, Network, NetworkStatus, Page, Person, Presence,
    Profile, Reaction, RelateAction, Relation, Request, Response, SendState, Watching,
    MUTUAL_SPACES, PAGE_LIMIT, SEARCH_MIN_CHARS, WATCH_LIMIT,
};
use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

/// How long `wait` holds a request open when nothing changes.
const WAIT: Duration = Duration::from_secs(30);
/// How long an adapter has to answer a command the shell is waiting on.
const ANSWER: Duration = Duration::from_secs(30);
/// A message still pending after this many seconds did not go.
const SEND_SECONDS: i64 = 60;
/// Most conversations of a Space's tree in one answer; they are small rows.
const TREE_LIMIT: u32 = 500;
/// A people search reaches a network once the typing has paused this long, so at most one
/// query per account in that time and never one per keystroke.
const SEARCH_PAUSE: Duration = Duration::from_millis(400);
/// The same query is not sent to an account again within this long: the shell reads `People`
/// again after every result, with the query still in the box.
const SEARCH_AGAIN: Duration = Duration::from_secs(60);
/// How long sign-out waits for the adapter to end the session on the network.
const SIGN_OUT: Duration = Duration::from_secs(5);
/// The largest pixel size a `Fetch` may ask a picture in.
const LARGEST: u32 = 4_096;

type Adapters = Arc<Mutex<HashMap<AccountKey, Adapter>>>;

/// People searches on their way to the networks: per account, the newest query and when it is
/// due, and the last one sent.
#[derive(Default)]
struct Searches {
    state: Mutex<Searching>,
    wake: Condvar,
}

#[derive(Default)]
struct Searching {
    waiting: HashMap<AccountKey, (String, Instant)>,
    sent: HashMap<AccountKey, (String, Instant)>,
}

impl Searches {
    /// Sends each query when its pause is over. With nothing waiting this sleeps on the condvar
    /// with no timeout, so an idle chatd is not woken for it.
    fn run(&self, adapters: &Adapters) {
        let mut state = lock(&self.state);
        loop {
            let at = Instant::now();
            let due: Vec<AccountKey> = state.waiting.iter().filter(|(_, (_, due))| *due <= at).map(|(account, _)| account.clone()).collect();
            for account in due {
                let Some((query, _)) = state.waiting.remove(&account) else {
                    continue;
                };
                state.sent.retain(|_, (_, sent)| sent.elapsed() < SEARCH_AGAIN);
                state.sent.insert(account.clone(), (query.clone(), at));
                if let Some(adapter) = lock(adapters).get(&account) {
                    adapter.command(Command::SearchPeople(query));
                }
            }
            state = match state.waiting.values().map(|(_, due)| *due).min() {
                Some(next) => self.wake.wait_timeout(state, next.saturating_duration_since(at)).unwrap_or_else(PoisonError::into_inner).0,
                None => self.wake.wait(state).unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    fn ask(&self, account: AccountKey, query: &str) {
        let mut state = lock(&self.state);
        let asked = |held: Option<&(String, Instant)>| held.is_some_and(|(sent, at)| sent == query && at.elapsed() < SEARCH_AGAIN);
        if asked(state.sent.get(&account)) {
            state.waiting.remove(&account);
            return;
        }
        // The pause runs from when the query last changed: asking for the same one again, as a
        // shell does on every repaint, does not put it off.
        if state.waiting.get(&account).is_some_and(|(waiting, _)| waiting == query) {
            return;
        }
        state.waiting.insert(account, (query.to_owned(), Instant::now() + SEARCH_PAUSE));
        drop(state);
        self.wake.notify_all();
    }
}

pub struct Chat {
    hub: Arc<Hub>,
    adapters: Adapters,
    blobs: Blobs,
    searches: Arc<Searches>,
    /// Itself, for the work a request starts that outlives the request (`ReadSessions`).
    this: OnceLock<Weak<Chat>>,
}

/// A refusal on its way out of a helper. Boxed, because a `Response` is large.
type Refused = Box<Response>;

fn error(code: ErrorCode, detail: impl ToString) -> Response {
    Response::Error {
        code,
        detail: detail.to_string(),
    }
}

fn internal(err: rusqlite::Error) -> Response {
    error(ErrorCode::Internal, err)
}

fn call_error(err: CallError) -> Response {
    match err {
        CallError::Active => error(ErrorCode::CallActive, "already in a call"),
        CallError::NoCall => error(ErrorCode::NoCall, "not in a call"),
        CallError::Media(err) => error(ErrorCode::Media, err),
    }
}

/// `TINYCLIENT_LOGIN`, or the `tinyclient-login` next to this binary.
fn login_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("TINYCLIENT_LOGIN") {
        return Ok(PathBuf::from(path));
    }
    Ok(std::env::current_exe()?.with_file_name(format!("tinyclient-login{}", std::env::consts::EXE_SUFFIX)))
}

/// `TINYCLIENT_TEST_KEYCHAIN_MS`: in a scratch directory, a session read takes this long, as one
/// held up by a keychain prompt does. Never read otherwise.
fn test_keychain_delay() -> Option<Duration> {
    if session::enabled() {
        return None;
    }
    std::env::var("TINYCLIENT_TEST_KEYCHAIN_MS").ok()?.parse().ok().map(Duration::from_millis)
}

/// `TINYCLIENT_TEST_PANIC`: in a scratch directory, a `Find` for this query panics while it holds
/// the store, as a bug in one request would. Never read otherwise.
fn test_panic(query: &str) -> bool {
    !session::enabled() && std::env::var("TINYCLIENT_TEST_PANIC").is_ok_and(|panics| panics == query)
}

impl Chat {
    /// Everything but the sessions: those are read by [`serve`] once the socket answers
    /// (`read_sessions`), because a keychain read can wait on the user for as long as they like.
    pub fn new(store: Store) -> Chat {
        let hub = Arc::new(Hub::new(store));
        let chat = Chat {
            blobs: Blobs::new(hub.clone(), support_dir().join("media")),
            hub,
            adapters: Arc::default(),
            searches: Arc::default(),
            this: OnceLock::new(),
        };
        // A call that fails here (mediad died, a deadline passed) is still up on the network
        // until the adapter leaves: the other side would go on ringing, the channel go on
        // showing us.
        let adapters = chat.adapters.clone();
        chat.hub.calls.on_failure(move |conversation| {
            if let Some(adapter) = lock(&adapters).get(&conversation.account()) {
                adapter.command(Command::CallLeave(conversation.id.clone()));
            }
        });
        let (searches, adapters) = (chat.searches.clone(), chat.adapters.clone());
        let spawned = std::thread::Builder::new().name("search".into()).stack_size(256 * 1024).spawn(move || searches.run(&adapters));
        if let Err(err) = spawned {
            eprintln!("chatd: people search: {err}");
        }
        // A seeded demo serves its own accounts.
        lock(&chat.adapters).extend(crate::demo::attach(&chat.hub));
        // Whatever was on its way when chatd last stopped did not go.
        let _ = chat.hub.store().fail_pending(i64::MAX, "tinyclient was restarted before this was sent.");
        // Retention runs behind the socket, at start, once a day, and when its setting changes.
        // A sleeping thread costs nothing in between.
        let hub = chat.hub.clone();
        std::thread::spawn(move || loop {
            let days = hub.settings().number(settings::KEEP_DAYS);
            match hub.store().prune(now(), days) {
                Ok(0) | Err(_) => {}
                Ok(gone) => {
                    eprintln!("chatd: pruned {gone} old messages");
                    // What is on screen may be gone.
                    hub.reset();
                }
            }
            hub.prune_due(Duration::from_secs(86_400));
        });
        chat
    }

    /// Reads each network's session from the keychain and connects its accounts, on a thread of
    /// its own, while the socket answers. `Status.keychain` says what it is doing; a read that
    /// sits behind a macOS prompt is `waiting`. After the first read of every network, the
    /// cache of accounts with no session left goes.
    pub fn read_sessions(chat: &Arc<Chat>, networks: Vec<Network>, first: bool) {
        let networks: Vec<Network> = networks.into_iter().filter(|network| adapters::supported(*network)).collect();
        chat.hub.reading(&networks);
        let chat = chat.clone();
        let spawned = std::thread::Builder::new().name("sessions".into()).stack_size(256 * 1024).spawn(move || {
            // The networks whose accounts are known for certain: no session at all, or a session
            // whose every account started. Only their leftovers are forgotten. A start that
            // failed, a refused keychain or a network this build does not speak keeps its cache.
            let mut settled = Vec::new();
            for network in networks {
                match chat.read_session(network) {
                    session::Stored::Found(session) => {
                        match chat.connect(network, &session) {
                            Ok(()) => settled.push(network),
                            Err(err) => eprintln!("chatd: {}: {err}; keeping its cache", network.label()),
                        }
                        chat.hub.read_now(network, false, false);
                    }
                    session::Stored::Absent => {
                        settled.push(network);
                        chat.hub.read_now(network, false, false);
                    }
                    // A refused keychain prompt is not a sign-out. What was cached stays, and
                    // `ReadSessions` (or the next start) asks again.
                    session::Stored::Unreadable(why) => {
                        eprintln!("chatd: {}: the keychain did not hand over the session ({why}); keeping its cache", network.label());
                        chat.hub.read_now(network, false, true);
                    }
                }
            }
            if first {
                chat.forget_strangers(&settled);
            }
        });
        if let Err(err) = spawned {
            eprintln!("chatd: sessions: {err}");
        }
    }

    /// One network's session. The read itself runs on a thread of its own so that one not
    /// answered within [`KEYCHAIN_PROMPT`] can be told as a prompt on screen while it waits.
    fn read_session(&self, network: Network) -> session::Stored {
        self.hub.read_now(network, true, false);
        let (done, read) = channel();
        let spawned = std::thread::Builder::new().name("keychain".into()).stack_size(128 * 1024).spawn(move || {
            if let Some(delay) = test_keychain_delay() {
                std::thread::sleep(delay);
            }
            let _ = done.send(session::read_waiting(network));
        });
        if let Err(err) = spawned {
            return session::Stored::Unreadable(err.to_string());
        }
        match read.recv_timeout(KEYCHAIN_PROMPT) {
            Ok(stored) => stored,
            Err(_) => {
                self.hub.read_waiting();
                read.recv().unwrap_or_else(|_| session::Stored::Unreadable("the keychain read ended without an answer".to_owned()))
            }
        }
    }

    /// Starts an adapter for every account in a network's session.
    fn connect(&self, network: Network, session: &str) -> Result<(), String> {
        for signed in adapters::accounts(network, session)? {
            let account = AccountKey {
                network,
                account: signed.account,
            };
            let named = self.hub.store().set_account(&account, &signed.name, "");
            named.map_err(|err| err.to_string())?;
            let adapter = adapters::start(account.clone(), &signed.session, self.hub.clone())?;
            lock(&self.adapters).insert(account, adapter);
        }
        Ok(())
    }

    /// Drops the cache of accounts that no longer have a session: signed out while chatd was
    /// down, or renamed by a newer adapter. A scratch directory has no sessions at all and keeps
    /// what was seeded into it.
    fn forget_strangers(&self, settled: &[Network]) {
        if !session::enabled() {
            return;
        }
        let stored = self.hub.store().accounts().unwrap_or_default();
        for account in stored.into_iter().map(|row| row.key) {
            if !settled.contains(&account.network) {
                continue;
            }
            if !lock(&self.adapters).contains_key(&account) {
                self.forget(&account);
            }
        }
    }

    fn forget(&self, account: &AccountKey) {
        self.hub.player.forget(&self.hub, account);
        self.blobs.forget(account);
        if let Err(err) = self.hub.store().forget(account) {
            eprintln!("chatd: {}: {err}", account.network.label());
        }
        self.hub.reload_settings();
        self.hub.forget_live(account);
    }

    /// Stores a setting and has whatever reads it take it up now.
    fn set(&self, scope: String, key: String, value: Option<String>) -> Response {
        let allowed = match &value {
            Some(value) => settings::valid(&scope, &key, value),
            None => settings::known(&scope, &key),
        };
        if !allowed {
            return error(ErrorCode::Refused, format!("{key} cannot be {value:?} here"));
        }
        if let Err(err) = self.hub.set_setting(&scope, &key, value) {
            return internal(err);
        }
        match key.as_str() {
            settings::KEEP_DAYS => self.hub.prune_soon(),
            settings::CACHE_MB => self.blobs.cap_changed(),
            settings::SOUNDS
            | settings::CUE_JOINS
            | settings::CUE_MINE
            | settings::CUE_CONNECTION
            | settings::CUE_VOLUME
            | settings::VOICE_VOLUME
            | settings::SENSITIVITY => self.hub.calls.retune(&self.hub),
            _ => {}
        }
        Response::Ok
    }

    /// Reset all (B3). The cache cap and retention go back to their defaults with the rest.
    fn reset_settings(&self) -> Response {
        if let Err(err) = self.hub.reset_settings() {
            return internal(err);
        }
        self.blobs.cap_changed();
        self.hub.prune_soon();
        self.hub.calls.retune(&self.hub);
        Response::Ok
    }

    /// Bytes on this Mac: the database with its journal, and the cached files.
    fn storage(&self) -> Response {
        let dir = support_dir();
        let size = |name: &str| std::fs::metadata(dir.join(name)).map_or(0, |meta| meta.len());
        let messages = size("chat.sqlite") + size("chat.sqlite-wal") + size("chat.sqlite-shm");
        Response::Storage { messages, files: self.blobs.bytes() }
    }

    /// Clear (D3): the files, then the messages, then every window reads what is left.
    fn clear(&self) -> Response {
        self.blobs.clear();
        if let Err(err) = self.hub.store().clear_cache() {
            return internal(err);
        }
        self.hub.reset();
        Response::Ok
    }

    /// chatd stops: the call is left on the network first, and the answer goes out before the
    /// process does.
    fn shutdown(&self) -> Response {
        if let Some(conversation) = self.hub.calls.end(&self.hub) {
            self.command(&conversation.account(), Command::CallLeave(conversation.id));
        }
        let spawned = std::thread::Builder::new().name("shutdown".into()).stack_size(64 * 1024).spawn(|| {
            // Time for this answer to be written and the adapter to say goodbye.
            std::thread::sleep(Duration::from_millis(300));
            eprintln!("chatd: asked to stop, exiting");
            std::process::exit(0);
        });
        if spawned.is_err() {
            std::process::exit(0);
        }
        Response::Ok
    }

    fn command(&self, account: &AccountKey, command: Command) {
        if let Some(adapter) = lock(&self.adapters).get(account) {
            adapter.command(command);
        }
    }

    /// Asks an account's adapter something and waits for its answer.
    fn ask<T>(&self, account: &AccountKey, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, Refused> {
        let (reply, answer) = channel();
        match lock(&self.adapters).get(account) {
            Some(adapter) => adapter.command(command(reply)),
            None => {
                let why = format!("{} is not signed in", account.network.label());
                return Err(Box::new(error(ErrorCode::Unsupported, why)));
            }
        }
        let refused = match answer.recv_timeout(ANSWER) {
            Ok(Ok(answer)) => return Ok(answer),
            Ok(Err(Refusal::Unsupported)) => error(
                ErrorCode::Unsupported,
                format!("{} cannot do this yet", account.network.label()),
            ),
            Ok(Err(Refusal::Refused(why))) => error(ErrorCode::Refused, why),
            Err(_) => error(ErrorCode::Offline, "the network did not answer"),
        };
        Err(Box::new(refused))
    }

    fn status(&self) -> Response {
        let (call, running) = self.hub.calls.describe();
        let links = self.hub.links();
        let keychain = self.hub.keychain();
        let pending = self.hub.sessions_pending();
        let stored = self.hub.store().accounts().unwrap_or_default();
        let mut accounts: Vec<AccountStatus> = stored
            .into_iter()
            .map(|row| {
                let linked = links.iter().find(|linked| linked.account == row.key);
                let (presence, status) = match row.me.as_str() {
                    "" => (Presence::Unknown, String::new()),
                    me => self.hub.shown(&Key::new(&row.key, me)),
                };
                // An account whose session is still being read, or was refused, is kept on
                // screen as offline with the reason, never dropped as signed out.
                let network = row.key.network;
                let unread_session = match linked {
                    Some(_) => None,
                    None if keychain.denied.contains(&network) => Some(session::STORE_REFUSED),
                    None if keychain.waiting == Some(network) => Some(session::STORE_WAITING),
                    None if pending.contains(&network) => Some(""),
                    None => None,
                };
                AccountStatus {
                    link: match unread_session {
                        Some(_) => Link::Offline,
                        None => linked.map_or(Link::SignedOut, |linked| linked.link),
                    },
                    detail: match unread_session {
                        Some(words) => words.to_owned(),
                        None => linked.map_or_else(String::new, |linked| linked.detail.clone()),
                    },
                    reconnecting: linked.is_some_and(|linked| linked.reconnecting),
                    required: linked.map_or_else(String::new, |linked| linked.required.clone()),
                    key: row.key,
                    name: row.name,
                    me: row.me,
                    attention: row.attention,
                    unread: row.unread,
                    avatar: row.avatar,
                    presence,
                    status,
                    can: linked.map_or_else(Capabilities::default, |linked| linked.can),
                }
            })
            .collect();
        accounts.retain(|account| account.link != Link::SignedOut || !session::enabled());
        Response::Status {
            networks: Network::ALL
                .into_iter()
                .map(|network| NetworkStatus {
                    network,
                    link: self.hub.network_link(network),
                })
                .collect(),
            accounts,
            media: if running { Media::Running } else { Media::Stopped },
            call,
            seq: self.hub.seq(),
            keychain,
            started: self.hub.started,
        }
    }

    /// Whether this account's cached chats and messages may be shown. A signed-out account keeps
    /// them on disk so the next sign-in has them, and the window is shown none of them. A
    /// keychain read still in progress is not a sign-out. Scratch directories have no sessions
    /// and keep showing what was seeded.
    fn shows(&self, account: &AccountKey) -> bool {
        if !session::enabled() {
            return true;
        }
        if self.hub.link(account) != Link::SignedOut {
            return true;
        }
        let network = account.network;
        let keychain = self.hub.keychain();
        keychain.denied.contains(&network)
            || keychain.waiting == Some(network)
            || self.hub.sessions_pending().contains(&network)
    }

    /// Accounts whose stored rows must not be listed.
    fn withheld(&self) -> Vec<AccountKey> {
        if !session::enabled() {
            return Vec::new();
        }
        self.hub
            .store()
            .accounts()
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.key)
            .filter(|key| !self.shows(key))
            .collect()
    }

    /// A row as the shell is told it: what is stored, and who is in it right now.
    fn row(&self, mut conversation: Conversation) -> Conversation {
        if conversation.kind == ConversationKind::Voice || conversation.call == CallSupport::Native {
            conversation.occupants = self.hub.occupants(&conversation.key);
        }
        conversation
    }

    fn rows(&self, conversations: Vec<Conversation>) -> Vec<Conversation> {
        conversations.into_iter().map(|conversation| self.row(conversation)).collect()
    }

    /// People as the shell is told them: what is stored, and what they are doing now.
    fn with_presence(&self, mut people: Vec<Person>) -> Vec<Person> {
        for person in &mut people {
            let doing = self.hub.doing(&person.key);
            person.presence = doing.presence;
            person.emoji = doing.emoji;
            person.activity = doing.activity.map(Box::new);
            if !doing.status.is_empty() {
                person.status = doing.status;
            }
        }
        people
    }

    fn people(
        &self,
        network: Option<Network>,
        relation: Option<Relation>,
        query: Option<String>,
        offset: u32,
        limit: u32,
    ) -> Response {
        let query = query.map(|query| query.trim().to_owned()).filter(|query| !query.is_empty());
        let found = self.hub.store().people(network, relation, query.as_deref(), offset, limit.min(PAGE_LIMIT));
        if let (Some(query), 0) = (&query, offset) {
            // The cache answers at once; the network's own search lands as a `People` change,
            // once the typing has paused and only for a query long enough to mean something.
            let worth = query.chars().count() >= SEARCH_MIN_CHARS;
            for linked in self.hub.links() {
                let account = linked.account;
                if worth && linked.link == Link::Connected && network.is_none_or(|network| network == account.network) {
                    self.searches.ask(account, query);
                }
            }
        }
        match found {
            Ok((people, total)) => Response::People {
                people: self.with_presence(people),
                total,
            },
            Err(err) => internal(err),
        }
    }

    fn watch(&self, mut watching: Watching) -> Response {
        watching.people.truncate(WATCH_LIMIT);
        watching.rows.truncate(protocol::ROWS_WATCHED);
        self.fill_previews(&watching.rows);
        let people = {
            let store = self.hub.store();
            let known = watching.people.iter().filter_map(|person| store.person(person).ok().flatten());
            known.collect()
        };
        let typists = watching.conversation.as_ref().map_or_else(Vec::new, |open| self.hub.typists(open));
        if self.hub.watch(watching) {
            let watching = self.hub.watching();
            for adapter in lock(&self.adapters).values() {
                adapter.command(Command::Watch(watching.clone()));
            }
        }
        Response::Watching {
            people: self.with_presence(people),
            typists,
        }
    }

    /// Rows on screen with no preview line are asked of their network, each once, paced by
    /// the adapter. Never on Discord: a fetch per DM is the burst that flags accounts there.
    fn fill_previews(&self, rows: &[Key]) {
        let wanted: Vec<Key> = rows.iter().filter(|row| row.network != Network::Discord).cloned().collect();
        if wanted.is_empty() {
            return;
        }
        let missing = self.hub.store().previewless(&wanted).unwrap_or_default();
        let mut by_account: HashMap<AccountKey, Vec<String>> = HashMap::new();
        for row in self.hub.ask_previews(missing) {
            by_account.entry(row.account()).or_default().push(row.id);
        }
        for (account, ids) in by_account {
            if self.hub.link(&account) == Link::Connected {
                self.command(&account, Command::Previews(ids));
            }
        }
    }

    fn messages(&self, conversation: Key, page: Page, limit: u32) -> Response {
        if !self.shows(&conversation.account()) {
            return Response::Messages {
                conversation,
                messages: Vec::new(),
                last_read: None,
                older: false,
                newer: false,
            };
        }
        let read = {
            let mut store = self.hub.store();
            let failed = store.fail_pending(now() - SEND_SECONDS, "The network did not answer.");
            for stuck in failed.iter().flatten().filter(|stuck| **stuck != conversation) {
                self.hub.changed(Change::Messages { conversation: stuck.clone() });
            }
            failed.and_then(|_| store.messages(&conversation, &page, limit.min(PAGE_LIMIT), now()))
        };
        match read {
            Ok(Some(found)) => {
                // What is stored is answered at once. The network catches it up behind that.
                let account = conversation.account();
                let conversation_id = conversation.id.clone();
                match &page {
                    Page::Newest => self.command(&account, Command::Backfill(conversation_id)),
                    Page::Before { id } if !found.older => {
                        let before = found.messages.first().map_or(id, |oldest| &oldest.id).clone();
                        self.command(&account, Command::History { conversation_id, before });
                    }
                    // The cache cannot see a gap in itself, so the adapter is asked each time
                    // and fetches only what it finds missing.
                    Page::After { id } => {
                        let after = id.clone();
                        self.command(&account, Command::HistoryAfter { conversation_id, after });
                    }
                    Page::Around { id } if !found.older || !found.newer || !found.messages.iter().any(|message| message.id == *id) => {
                        let around = id.clone();
                        self.command(&account, Command::HistoryAround { conversation_id, around });
                    }
                    _ => {}
                }
                Response::Messages {
                    conversation,
                    messages: found.messages,
                    last_read: found.last_read,
                    older: found.older,
                    newer: found.newer,
                }
            }
            Ok(None) => error(ErrorCode::NotFound, "no such conversation"),
            Err(err) => internal(err),
        }
    }

    fn mark_read(&self, conversation: Key, message_id: String) -> Response {
        // The store is let go before the settings are read: a guard made in a `match` lives
        // to its end, and `Hub::set_setting` takes the two the other way round.
        let marked = self.hub.store().mark_read(&conversation, &message_id);
        match marked {
            Ok(true) => {
                let conversation_id = conversation.id.clone();
                // P2: off, the chat is read here and the network is not told, which is what
                // others would see as a receipt.
                let account = conversation.account();
                if self.hub.settings().receipts(&account) {
                    self.command(&account, Command::MarkRead { conversation_id, message_id });
                }
                self.hub.changed(Change::Conversation { conversation });
                self.hub.changed(Change::Status);
                Response::Ok
            }
            Ok(false) => Response::Ok,
            Err(err) => internal(err),
        }
    }

    /// Stores the message as pending and hands it to the adapter. The answer does not wait for
    /// the network; the outcome arrives as a `Messages` change.
    fn send(&self, conversation: Key, mut message: Message, reply_to: Option<String>, with: Outgoing) -> Response {
        let account = conversation.account();
        if self.hub.link(&account) != Link::Connected {
            let what = if with.files.is_empty() { "" } else { " Files need a connection." };
            return error(ErrorCode::Offline, format!("{} is offline.{what}", account.network.label()));
        }
        if let Err(refused) = self.allowed(&account, &with) {
            return *refused;
        }
        let client_id = message.id.clone();
        message.state = SendState::Pending;
        message.sent = now();
        let stored = {
            let mut store = self.hub.store();
            match store.conversation(&conversation) {
                Ok(Some(_)) => store.ingest(&conversation, &message, false),
                Ok(None) => return error(ErrorCode::NotFound, "no such conversation"),
                Err(err) => Err(err),
            }
        };
        if let Err(err) = stored {
            return internal(err);
        }
        self.hub.changed(Change::Messages { conversation: conversation.clone() });
        self.hub.changed(Change::Conversation { conversation: conversation.clone() });
        let (conversation_id, text) = (conversation.id, message.text.clone());
        let command = match with.is_plain() {
            true => Command::Send { conversation_id, client_id, text, reply_to },
            false => Command::SendWith { conversation_id, client_id, text, reply_to, with: Box::new(with) },
        };
        self.command(&account, command);
        Response::Sent { message }
    }

    /// What the account may send, checked before anything is stored: a file too large or too
    /// many is refused here, naming the limit, and the network hears nothing.
    fn allowed(&self, account: &AccountKey, with: &Outgoing) -> Result<(), Refused> {
        let refuse = |why: String| Err(Box::new(error(ErrorCode::Refused, why)));
        let name = account.network.label();
        let can = self.hub.capabilities(account);
        if with.files.is_empty() {
            return Ok(());
        }
        if can.max_file_bytes == 0 {
            return refuse(format!("Files can't be sent to {name} from tinyclient yet."));
        }
        let most = (can.max_files.max(1) as usize).min(protocol::ATTACHMENTS_LIMIT);
        if with.files.len() > most {
            return refuse(format!("{name} takes at most {most} files in one message."));
        }
        for file in &with.files {
            let named = file_name(file);
            // The same rule the upload reads by: never chatd's own files, a device, a secret, or
            // a link in `outbox/`.
            let real = match crate::blobs::upload::allowed(&file.path) {
                Ok((real, _)) => real,
                Err(why) => return refuse(format!("{named} {why}")),
            };
            match std::fs::metadata(&real) {
                Ok(meta) if meta.is_dir() => return refuse(format!("{named}: folders can't be sent. Zip it first.")),
                Ok(meta) if meta.len() > can.max_file_bytes => {
                    return refuse(format!("{named} is too big for {name} (limit {}).", megabytes(can.max_file_bytes)))
                }
                Ok(_) => {}
                Err(_) => return refuse(format!("{named} is no longer there.")),
            }
        }
        Ok(())
    }

    fn new_message(&self, conversation: &Key, client_id: String, text: String, reply_to: Option<&str>, with: &Outgoing) -> Message {
        let mut message = Message::plain(&client_id, "You", text, now(), true);
        // The files are listed at once, from where they are, so the pending row shows them.
        message.attachments = with
            .files
            .iter()
            .take(protocol::ATTACHMENTS_LIMIT)
            .enumerate()
            .map(|(at, file)| {
                let bytes = std::fs::metadata(&file.path).map_or(0, |meta| meta.len());
                let id = format!("upload-{client_id}-{at}");
                let mut attached = protocol::Attachment::new(
                    file_name(file),
                    MediaFile { bytes, path: Some(file.path.clone()), ..MediaFile::new(id, kind_of(&file.path)) },
                );
                attached.spoiler = file.spoiler;
                attached
            })
            .collect();
        message.client_id = Some(client_id);
        let store = self.hub.store();
        let quoted = reply_to.and_then(|id| store.message(conversation, id).ok().flatten());
        message.reply_to = quoted.map(|quoted| protocol::ReplyTo {
            id: quoted.id,
            person: (!quoted.sender.id.is_empty()).then(|| Key::new(&conversation.account(), quoted.sender.id)),
            sender: quoted.sender.name,
            snippet: quoted.text.chars().take(120).collect(),
            deleted: quoted.deleted,
        });
        message
    }

    /// Refuses what needs the network when the account is not connected to it.
    fn online(&self, account: &AccountKey) -> Result<(), Refused> {
        match self.hub.link(account) {
            Link::Connected => Ok(()),
            _ => Err(Box::new(error(ErrorCode::Offline, format!("{} is offline", account.network.label())))),
        }
    }

    /// What reactions a conversation takes, kept five minutes. `None` when the adapter does not
    /// say, or could not ask: then the network alone judges.
    fn reaction_rules(&self, conversation: &Key) -> Result<protocol::ReactionRules, Refused> {
        if let Some(rules) = self.hub.reactions.get(conversation) {
            return Ok(rules);
        }
        let conversation_id = conversation.id.clone();
        let mut rules = self.ask(&conversation.account(), |reply| Command::ReactionRules { conversation_id, reply })?;
        rules.bound();
        self.hub.reactions.keep(conversation.clone(), rules.clone());
        Ok(rules)
    }

    fn reactions(&self, conversation: Key) -> Response {
        match self.reaction_rules(&conversation) {
            Ok(rules) => Response::Reactions { conversation, rules },
            Err(refused) => *refused,
        }
    }

    /// Our reaction, shown at once and taken back if the network refuses it. One the
    /// conversation's rules refuse is refused here and never sent; one that replaces mine (a
    /// network with one reaction a person) takes mine off at once too. A custom emoji that is
    /// not on the message yet has no name here to draw, so that one waits for the network's word.
    fn react(&self, conversation: Key, message_id: String, emoji: String, on: bool) -> Response {
        let account = conversation.account();
        if let Err(refused) = self.online(&account) {
            return *refused;
        }
        let ours = |emoji: &str| Reaction {
            emoji: emoji.to_owned(),
            file: None,
            count: 1,
            mine: true,
        };
        let message = match self.hub.store().message(&conversation, &message_id) {
            Ok(Some(message)) => message,
            Ok(None) => return error(ErrorCode::NotFound, "no such message"),
            Err(err) => return internal(err),
        };
        let known = self.hub.store().media(&account, std::slice::from_ref(&emoji)).is_ok_and(|known| !known.is_empty());
        // Rules the adapter cannot give leave it to the network, as before there were any.
        let (emoji, replacing) = match (on, self.reaction_rules(&conversation)) {
            (true, Ok(rules)) => match rules.check(&message.reactions, &emoji, known || emoji.starts_with(':')) {
                // Sent as the network spells it, so its own word of it is the same reaction.
                Ok(replacing) => (rules.spelled(&emoji).to_owned(), replacing),
                Err(why) => return error(ErrorCode::Refused, why),
            },
            _ => (emoji, Vec::new()),
        };
        let shown = {
            let mut store = self.hub.store();
            for old in &replacing {
                if let Err(err) = store.react(&conversation, &message_id, &ours(old), false) {
                    return internal(err);
                }
            }
            let on_it = message.reactions.iter().any(|have| have.emoji == emoji || have.file.as_ref().is_some_and(|file| file.id == emoji));
            if known && !on_it {
                false
            } else {
                match store.react(&conversation, &message_id, &ours(&emoji), on) {
                    Ok(changed) => changed,
                    Err(err) => return internal(err),
                }
            }
        };
        if shown || !replacing.is_empty() {
            self.hub.changed(Change::Messages { conversation: conversation.clone() });
        }
        let (conversation_id, id) = (conversation.id.clone(), message_id.clone());
        let asked = self.ask(&account, |reply| Command::React {
            conversation_id,
            message_id: id,
            emoji: emoji.clone(),
            on,
            reply,
        });
        match asked {
            Ok(()) => Response::Ok,
            Err(refused) => {
                // What the network refused may be what the kept rules allowed: ask again next time.
                self.hub.reactions.forget(&conversation);
                let mut store = self.hub.store();
                let mut undone = shown && store.react(&conversation, &message_id, &ours(&emoji), !on).unwrap_or(false);
                for old in &replacing {
                    let had = message.reactions.iter().find(|have| have.emoji == *old).map(|have| Reaction { count: 1, ..have.clone() });
                    undone |= store.react(&conversation, &message_id, &had.unwrap_or_else(|| ours(old)), true).unwrap_or(false);
                }
                drop(store);
                if undone {
                    self.hub.changed(Change::Messages { conversation });
                }
                *refused
            }
        }
    }

    /// Edit and delete change nothing here: the answer is the network's, and the message
    /// changes when the adapter reports it.
    fn rewrite(&self, conversation: Key, message_id: String, text: Option<String>) -> Response {
        let account = conversation.account();
        if let Err(refused) = self.online(&account) {
            return *refused;
        }
        let message = match self.hub.store().message(&conversation, &message_id) {
            Ok(Some(message)) => message,
            Ok(None) => return error(ErrorCode::NotFound, "no such message"),
            Err(err) => return internal(err),
        };
        if message.state != SendState::Sent {
            return error(ErrorCode::Refused, "This message was never sent. Discard it instead.");
        }
        let conversation_id = conversation.id;
        let asked = match text {
            Some(_) if !message.outgoing => return error(ErrorCode::Refused, "Only your own messages can be edited."),
            Some(text) if text.trim().is_empty() => return error(ErrorCode::Refused, "A message cannot be edited to nothing. Delete it instead."),
            Some(text) => self.ask(&account, |reply| Command::Edit {
                conversation_id,
                message_id,
                text,
                reply,
            }),
            None => self.ask(&account, |reply| Command::Delete {
                conversation_id,
                message_id,
                reply,
            }),
        };
        match asked {
            Ok(()) => Response::Ok,
            Err(refused) => *refused,
        }
    }

    fn relate(&self, person: Key, action: RelateAction) -> Response {
        let account = person.account();
        if let Err(refused) = self.online(&account) {
            return *refused;
        }
        let person_id = person.id;
        match self.ask(&account, |reply| Command::Relate { person_id, action, reply }) {
            Ok(()) => Response::Ok,
            Err(refused) => *refused,
        }
    }

    /// Our own presence, shown at once and put back if the network refuses it.
    fn set_presence(&self, account: AccountKey, presence: Presence, status: Option<String>) -> Response {
        if let Err(refused) = self.online(&account) {
            return *refused;
        }
        let stored = self.hub.store().accounts().unwrap_or_default();
        let me = stored.into_iter().find(|row| row.key == account).map(|row| row.me).filter(|me| !me.is_empty());
        let me = me.map(|me| Key::new(&account, me));
        let status = status.map(|mut status| {
            clip(&mut status, 128);
            status
        });
        let before = me.as_ref().map(|me| self.hub.shown(me));
        if let (Some(me), Some((_, old))) = (&me, &before) {
            self.hub.set_presence(me.clone(), presence, status.as_deref().unwrap_or(old), true);
            self.hub.changed(Change::Status);
        }
        match self.ask(&account, |reply| Command::SetPresence { presence, status, reply }) {
            Ok(()) => Response::Ok,
            Err(refused) => {
                if let (Some(me), Some((presence, status))) = (me, before) {
                    self.hub.set_presence(me, presence, &status, true);
                    self.hub.changed(Change::Status);
                }
                *refused
            }
        }
    }

    /// One person's card: asked of the network once, kept five minutes. Opened from a Space,
    /// the adapter is asked for what is said of them there, and failing that for the card alone.
    fn profile(&self, person: Key, space: Option<Key>) -> Response {
        let known = match self.hub.store().person(&person) {
            Ok(Some(known)) => known,
            Ok(None) => return error(ErrorCode::NotFound, "no such person"),
            Err(err) => return internal(err),
        };
        let cached = (person.clone(), space.clone());
        if let Some(mut profile) = self.hub.profiles.get(&cached) {
            profile.person = self.with_presence(vec![known]).remove(0);
            return Response::Profile { profile: Box::new(profile) };
        }
        let account = person.account();
        let space_id = space.filter(|space| space.account() == account).map(|space| space.id);
        let asked = match space_id {
            Some(space_id) => {
                let person_id = person.id.clone();
                match self.ask(&account, |reply| Command::ProfileIn { person_id, space_id, reply }) {
                    Err(refused) if matches!(*refused, Response::Error { code: ErrorCode::Unsupported, .. }) => None,
                    asked => Some(asked),
                }
            }
            None => None,
        };
        let asked = asked.unwrap_or_else(|| {
            let person_id = person.id.clone();
            self.ask(&account, |reply| Command::Profile { person_id, reply })
        });
        let about = match asked {
            Ok(about) => about,
            Err(refused) => return *refused,
        };
        let profile = typed_profile(self.with_presence(vec![known]).remove(0), about);
        self.hub.profiles.keep(cached, profile.clone());
        Response::Profile { profile: Box::new(profile) }
    }

    /// A conversation's info: its row, and what the network says beyond it, kept five minutes.
    fn conversation_info(&self, conversation: Key) -> Response {
        let row = match self.hub.store().conversation(&conversation) {
            Ok(Some(row)) => self.row(row),
            Ok(None) => return error(ErrorCode::NotFound, "no such conversation"),
            Err(err) => return internal(err),
        };
        if let Some(mut info) = self.hub.infos.get(&conversation) {
            info.conversation = row;
            return Response::ConversationInfo { info: Box::new(info) };
        }
        let conversation_id = conversation.id.clone();
        let about = match self.ask(&conversation.account(), |reply| Command::ConversationInfo { conversation_id, reply }) {
            Ok(about) => about,
            Err(refused) => return *refused,
        };
        let mut info = ConversationInfo {
            conversation: row,
            description: about.description,
            members: about.members,
            online: about.online,
            created: about.created,
            creator: about.creator,
            notify: about.notify,
            pinned: about.pinned,
            external: about.external,
            link: about.link,
        };
        clip(&mut info.description, 4_000);
        clip(&mut info.creator, 100);
        clip(&mut info.external, 100);
        clip(&mut info.link, 300);
        self.hub.infos.keep(conversation, info.clone());
        Response::ConversationInfo { info: Box::new(info) }
    }

    fn space_info(&self, space: Key) -> Response {
        let stored = self.hub.store().spaces(Some(space.network)).map(|spaces| spaces.into_iter().find(|known| known.key == space));
        let known = match stored {
            Ok(Some(known)) => known,
            Ok(None) => return error(ErrorCode::NotFound, "no such space"),
            Err(err) => return internal(err),
        };
        if let Some(mut info) = self.hub.space_infos.get(&space) {
            info.space = known;
            return Response::SpaceInfo { info: Box::new(info) };
        }
        let space_id = space.id.clone();
        let about = match self.ask(&space.account(), |reply| Command::SpaceInfo { space_id, reply }) {
            Ok(about) => about,
            Err(refused) => return *refused,
        };
        let mut info = SpaceInfo {
            space: known,
            description: about.description,
            members: about.members,
            online: about.online,
            nickname: about.nickname,
            roles: about.roles,
            notify: about.notify,
        };
        clip(&mut info.description, 4_000);
        clip(&mut info.nickname, 100);
        info.roles.truncate(protocol::PROFILE_ROLES);
        info.roles.iter_mut().for_each(|role| clip(role, 100));
        self.hub.space_infos.keep(space, info.clone());
        Response::SpaceInfo { info: Box::new(info) }
    }

    /// Something the user did to a conversation on the network: mute, leave, close. The answer
    /// is the network's; the row changes when the adapter reports it.
    fn act(&self, conversation: &Key, command: impl FnOnce(String, Reply<()>) -> Command) -> Response {
        let account = conversation.account();
        if let Err(refused) = self.online(&account) {
            return *refused;
        }
        let conversation_id = conversation.id.clone();
        match self.ask(&account, |reply| command(conversation_id, reply)) {
            Ok(()) => {
                self.hub.infos.forget(conversation);
                Response::Ok
            }
            Err(refused) => *refused,
        }
    }

    fn pinned(&self, conversation: Key) -> Response {
        let account = conversation.account();
        if let Err(refused) = self.online(&account) {
            return *refused;
        }
        let conversation_id = conversation.id.clone();
        match self.ask(&account, |reply| Command::Pinned { conversation_id, reply }) {
            Ok(mut messages) => {
                messages.truncate(protocol::PINNED_LIMIT);
                messages.iter_mut().for_each(Message::bound);
                Response::Pinned { conversation, messages }
            }
            Err(refused) => *refused,
        }
    }

    fn catalog(&self, account: AccountKey, kind: CatalogKind, space: Option<Key>, query: String, cursor: Option<String>) -> Response {
        let mut query = query.trim().to_owned();
        clip(&mut query, 100);
        let asked = (account.clone(), kind, space.clone(), query.clone(), cursor.clone());
        if let Some((items, cursor)) = self.hub.catalogs.get(&asked) {
            return Response::Catalog { items, cursor };
        }
        if let Err(refused) = self.online(&account) {
            return *refused;
        }
        let space_id = space.filter(|space| space.account() == account).map(|space| space.id);
        match self.ask(&account, |reply| Command::Catalog { kind, space_id, query, cursor, reply }) {
            Ok(mut page) => {
                page.items.truncate(protocol::CATALOG_LIMIT);
                for item in &mut page.items {
                    clip(&mut item.name, 100);
                    clip(&mut item.pack, 100);
                }
                self.hub.catalogs.keep(asked, (page.items.clone(), page.cursor.clone()));
                Response::Catalog { items: page.items, cursor: page.cursor }
            }
            Err(refused) => *refused,
        }
    }

    /// Voice channels and calls with a friend in them now, the fullest first.
    fn in_voice(&self, network: Option<Network>) -> Response {
        let mut occupied = self.hub.occupied();
        occupied.retain(|(conversation, _)| network.is_none_or(|network| conversation.network == network));
        occupied.retain(|(conversation, _)| self.shows(&conversation.account()));
        occupied.sort_by_key(|(conversation, occupants)| (std::cmp::Reverse(occupants.total), conversation.id.clone()));
        let mut rows = Vec::new();
        {
            let store = self.hub.store();
            for (conversation, occupants) in occupied {
                let Ok(Some(row)) = store.conversation(&conversation) else {
                    continue;
                };
                let friend = occupants.people.iter().any(|occupant| {
                    store.person(&occupant.person).ok().flatten().is_some_and(|person| person.relation == Relation::Friend)
                });
                if friend || row.kind.is_direct() {
                    rows.push(row);
                }
                if rows.len() >= 50 {
                    break;
                }
            }
        }
        let total = rows.len() as u32;
        Response::Conversations { conversations: self.rows(rows), total, counts: InboxCounts::default() }
    }

    /// Who could I talk to now: in a call, around, talked to lately. Bots never.
    fn around(&self, network: Option<Network>) -> Response {
        let read = {
            let store = self.hub.store();
            let owned = store.owned(network, 2_000);
            let talked = store.talked_to(network, now() - crate::store::RECENT_SECONDS, 500);
            let requests = store.incoming(network);
            owned.and_then(|owned| Ok((owned, talked?, requests?)))
        };
        let (owned, talked, requests) = match read {
            Ok(read) => read,
            Err(err) => return internal(err),
        };
        let (mut in_call, mut online, mut recent) = (Vec::new(), Vec::new(), Vec::new());
        let mut seen = std::collections::HashSet::new();
        let talked_ids: std::collections::HashSet<Key> = talked.iter().map(|person| person.key.clone()).collect();
        for person in self.with_presence(owned.into_iter().chain(talked).collect()) {
            if !self.shows(&person.key.account()) || !seen.insert(person.key.clone()) {
                continue;
            }
            if person.activity.as_ref().is_some_and(|activity| activity.kind.in_call()) {
                in_call.push(person);
            } else if person.presence.is_around() {
                online.push(person);
            } else if talked_ids.contains(&person.key) {
                recent.push(person);
            }
        }
        for section in [&mut in_call, &mut online, &mut recent] {
            section.sort_by_cached_key(|person| person.name.to_lowercase());
            section.truncate(protocol::AROUND_LIMIT);
        }
        Response::Around { requests, in_call, online, recent }
    }

    /// What was shared: from the cache, saying it is only that; or, because the user asked,
    /// from the network's own search.
    fn shared(&self, conversation: Key, kind: SharedKind, before: Option<String>, limit: u32, search: bool) -> Response {
        if !self.shows(&conversation.account()) {
            return error(ErrorCode::NotFound, "signed out");
        }
        let limit = limit.min(PAGE_LIMIT);
        if !search {
            return match self.hub.store().shared(&conversation, kind, before.as_deref(), limit) {
                Ok(Some((items, more))) => Response::Shared { conversation, items, more, complete: false, total: None },
                Ok(None) => error(ErrorCode::NotFound, "no such conversation"),
                Err(err) => internal(err),
            };
        }
        let account = conversation.account();
        if let Err(refused) = self.online(&account) {
            return *refused;
        }
        let conversation_id = conversation.id.clone();
        match self.ask(&account, |reply| Command::SearchShared { conversation_id, kind, before, limit, reply }) {
            Ok(mut page) => {
                page.items.truncate(limit as usize);
                Response::Shared { conversation, items: page.items, more: page.more, complete: true, total: page.total }
            }
            Err(refused) => *refused,
        }
    }

    /// The unsent message with this client id, if it is one the user may retry or discard.
    fn unsent(&self, conversation: &Key, client_id: &str) -> Result<Message, Refused> {
        match self.hub.store().message(conversation, client_id) {
            Ok(Some(message)) if message.state != SendState::Sent => Ok(message),
            Ok(_) => Err(Box::new(error(ErrorCode::NotFound, "no such unsent message"))),
            Err(err) => Err(Box::new(internal(err))),
        }
    }

    /// Shows the network's sign-in and waits for the user to finish it.
    fn sign_in(&self, network: Network, at: Option<(i32, i32)>) -> Response {
        if !adapters::supported(network) {
            return error(
                ErrorCode::Unsupported,
                format!("{} is not built yet", network.label()),
            );
        }
        // Signal links this computer from the phone, in the sheet the shell draws itself
        // (`SignInBegin`); it has no sign-in page for a window to show.
        if network == Network::Signal {
            return error(ErrorCode::Unsupported, "Signal signs in by linking from your phone, not in a window.");
        }
        // Tiny signs in through the sheet (`SignInBegin`), never the browser helper.
        if network == Network::Tiny {
            return error(ErrorCode::Unsupported, "A Tiny server signs in in the sheet, not in a window.");
        }
        // A scratch directory signs nobody in. A test may still name a helper of its own, to
        // see what it is run with and how its answer is told; its session is never kept.
        if !session::enabled() && std::env::var_os("TINYCLIENT_LOGIN").is_none() {
            return error(ErrorCode::SignIn, "sign-in is off while TINYCLIENT_DIR is set");
        }
        let helper = login_path().and_then(|path| {
            let mut helper = std::process::Command::new(path);
            protocol::launch::no_console(&mut helper).arg(network.label().to_lowercase());
            if let Some((x, y)) = at {
                helper.args(["--at", &format!("{x},{y}")]);
            }
            helper.stderr(std::process::Stdio::inherit()).output()
        });
        let name = network.label();
        let session = match helper {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            // The helper's own words for how it ended (`crates/login`).
            Ok(output) => {
                return match output.status.code() {
                    Some(1) => error(ErrorCode::SignIn, "The sign-in window was closed."),
                    Some(3) => error(ErrorCode::SignInUnreachable, format!("{name}'s sign-in page could not be loaded. Check the connection and try again.")),
                    Some(4) => error(ErrorCode::SignInBlocked, format!("Your organization does not allow signing in to {name} this way.")),
                    _ => error(ErrorCode::SignIn, "The sign-in window stopped before it finished."),
                }
            }
            Err(err) => return error(ErrorCode::SignIn, format!("sign-in could not start: {err}")),
        };
        // The new session replaces every adapter of the network: an expired account comes back,
        // and the accounts that were fine reconnect once.
        let started = session::save(network, &session).and_then(|()| {
            lock(&self.adapters).retain(|account, _| account.network != network);
            self.connect(network, &session)
        });
        match started {
            Ok(()) => self.status(),
            Err(why) => error(ErrorCode::SignIn, why),
        }
    }

    /// A sign-in the shell draws itself: begins it, or passes on what the user did, and answers
    /// with the step to draw. Whenever it ends signed in, `adopt` keeps the session and starts
    /// the account; that may be long after this answered (a QR code scanned later).
    fn sign_in_step(&self, network: Network, input: Option<protocol::SignInInput>) -> Response {
        let (hub, adapters) = (self.hub.clone(), self.adapters.clone());
        let adopt = move |session: &str| adopt(&hub, &adapters, network, session);
        let step = match network {
            Network::Telegram => adapters::telegram::signin::step(&self.hub, input, Box::new(adopt)),
            Network::Signal => adapters::signal::signin::step(&self.hub, input, Box::new(adopt)),
            Network::Messages => adapters::messages::signin::step(&self.hub, input, Box::new(adopt)),
            Network::Tiny => adapters::tiny::signin::step(&self.hub, input, Box::new(adopt)),
            _ => Err(format!("{} signs in in its own window.", network.label())),
        };
        match step {
            Ok(step) => Response::SignIn { network, step },
            Err(why) => error(ErrorCode::Unsupported, why),
        }
    }

    fn sign_out(&self, account: AccountKey) -> Response {
        // Taken out of the map first, so nothing else is asked of it; kept until it has had
        // its time to end the session on the network, since dropping it stops its threads.
        let adapter = lock(&self.adapters).remove(&account);
        if let Some(adapter) = adapter {
            let (reply, done) = channel();
            adapter.command(Command::SignOut(reply));
            // Answered, or dropped unanswered by an adapter with nothing to end, or too slow.
            let _ = done.recv_timeout(SIGN_OUT);
        }
        let stored = match session::read(account.network) {
            session::Stored::Found(session) => Some(session),
            session::Stored::Absent => None,
            // Forgetting the item now would sign out every other account it holds.
            session::Stored::Unreadable(why) => return error(ErrorCode::Internal, format!("The session could not be read from {}: {why}", session::STORE)),
        };
        let left = stored.and_then(|session| adapters::without(account.network, &session, &account.account));
        match left {
            Some(session) => {
                if let Err(why) = session::save(account.network, &session) {
                    if session::enabled() {
                        return error(ErrorCode::Internal, why);
                    }
                }
            }
            None => session::forget(account.network),
        }
        self.forget(&account);
        self.hub.set_link(&account, Link::SignedOut, "");
        self.hub.changed(Change::Status);
        self.status()
    }

    /// Only a conversation tinyclient can carry a call for starts one. Anything else is refused
    /// before mediad is spawned or the network hears a thing.
    fn call_start(&self, conversation: Key) -> Response {
        match self.hub.store().conversation(&conversation) {
            Ok(Some(row)) if row.call == CallSupport::Native => {}
            Ok(Some(row)) if row.call == CallSupport::Handoff => {
                return error(ErrorCode::Unsupported, format!("{} calls open in its own app.", conversation.network.label()))
            }
            Ok(Some(_)) => return error(ErrorCode::Unsupported, "This conversation has no calls tinyclient can join."),
            Ok(None) => return error(ErrorCode::NotFound, "no such conversation"),
            Err(err) => return internal(err),
        }
        if let Err(err) = self.hub.calls.start(&self.hub, conversation.clone()) {
            return call_error(err);
        }
        self.join(conversation);
        self.status()
    }

    /// Has the adapter join, muted when the user joins muted (V4).
    fn join(&self, conversation: Key) {
        let account = conversation.account();
        self.command(&account, Command::CallJoin(conversation.id));
        if self.hub.settings().on(settings::JOIN_MUTED) {
            self.command(&account, Command::CallAudio { muted: true, deafened: false });
        }
    }

    pub fn handle(&self, request: Request) -> Response {
        match request {
            Request::Ping => Response::Pong,
            Request::Status => self.status(),
            Request::Wait { since } => {
                let (seq, reset, changes) = self.hub.wait(since, WAIT);
                Response::Changes { seq, reset, changes }
            }

            Request::Inbox { network, filter, offset, limit } => {
                let hidden = self.withheld();
                let read = {
                    let store = self.hub.store();
                    let page = store.inbox(network, filter, offset, limit.min(PAGE_LIMIT), &hidden);
                    page.and_then(|(conversations, total)| Ok((conversations, total, store.inbox_counts(network, &hidden)?)))
                };
                match read {
                    Ok((conversations, total, counts)) => Response::Conversations {
                        conversations: self.rows(conversations),
                        total,
                        counts,
                    },
                    Err(err) => internal(err),
                }
            }
            Request::Home { network, account, folder, shelf, offset, limit } => {
                let hidden = self.withheld();
                let read = self.hub.store().home(network, account.as_ref(), folder.as_deref(), shelf, offset, limit.min(PAGE_LIMIT), &hidden);
                if offset == 0 {
                    for linked in self.hub.links() {
                        let mine = linked.account.network == network && account.as_ref().is_none_or(|account| *account == linked.account);
                        if mine && linked.link == Link::Connected {
                            self.command(&linked.account, Command::OpenHome);
                        }
                    }
                }
                match read {
                    Ok((conversations, total, counts)) => Response::Conversations { conversations: self.rows(conversations), total, counts },
                    Err(err) => internal(err),
                }
            }
            Request::Folders { network } => match self.hub.store().folders(Some(network)) {
                Ok(folders) => Response::Folders { folders },
                Err(err) => internal(err),
            },
            Request::Threads { space, offset, limit } => {
                if !self.shows(&space.account()) {
                    return Response::Conversations { conversations: Vec::new(), total: 0, counts: InboxCounts::default() };
                }
                let read = self.hub.store().threads(&space, offset, limit.min(PAGE_LIMIT));
                if offset == 0 {
                    self.command(&space.account(), Command::OpenThreads(space.id.clone()));
                }
                match read {
                    Ok((conversations, total)) => {
                        Response::Conversations { conversations: self.rows(conversations), total, counts: InboxCounts::default() }
                    }
                    Err(err) => internal(err),
                }
            }
            Request::Topics { forum, offset, limit } => {
                if !self.shows(&forum.account()) {
                    return Response::Conversations { conversations: Vec::new(), total: 0, counts: InboxCounts::default() };
                }
                match self.hub.store().topics(&forum, offset, limit.min(PAGE_LIMIT)) {
                    Ok((conversations, total)) => {
                        Response::Conversations { conversations: self.rows(conversations), total, counts: InboxCounts::default() }
                    }
                    Err(err) => internal(err),
                }
            }
            Request::InVoice { network } => self.in_voice(network),
            Request::Around { network } => self.around(network),
            Request::Catalog { account, kind, space, query, cursor } => self.catalog(account, kind, space, query, cursor),
            Request::ReadSessions => {
                let denied = self.hub.keychain().denied;
                if let Some(chat) = self.this.get().and_then(Weak::upgrade) {
                    if !denied.is_empty() {
                        Chat::read_sessions(&chat, denied, false);
                    }
                }
                self.status()
            }
            Request::Find { network, query, limit } => {
                let query = query.trim();
                if test_panic(query) {
                    let _held = self.hub.store();
                    panic!("a test asked this request to panic");
                }
                let found = match query.is_empty() {
                    true => Ok(Vec::new()),
                    false => self.hub.store().find(network, query, limit.min(PAGE_LIMIT)),
                };
                match found {
                    Ok(conversations) => {
                        let conversations: Vec<_> = conversations.into_iter().filter(|row| self.shows(&row.key.account())).collect();
                        Response::Conversations {
                            total: conversations.len() as u32,
                            conversations: self.rows(conversations),
                            counts: InboxCounts::default(),
                        }
                    }
                    Err(err) => internal(err),
                }
            }
            Request::SearchMessages { conversation, network, space, query, limit } => {
                let query = query.trim();
                let found = match query.is_empty() {
                    true => Ok(Vec::new()),
                    false => self.hub.store().search_messages(conversation.as_ref(), network, space.as_ref(), query, limit.min(PAGE_LIMIT)),
                };
                match found {
                    Ok(hits) => {
                        let hits: Vec<_> = hits.into_iter().filter(|hit| self.shows(&hit.conversation.account())).collect();
                        Response::MessageHits { hits }
                    }
                    Err(err) => internal(err),
                }
            }
            Request::SearchNetwork { conversation, space, query } => {
                let query = query.trim().to_owned();
                if query.is_empty() {
                    Response::MessageHits { hits: Vec::new() }
                } else {
                    let (account, scope) = match (&conversation, &space) {
                        (Some(key), _) => (key.account(), SearchScope::Conversation(key.id.clone())),
                        (None, Some(key)) => (key.account(), SearchScope::Space(key.id.clone())),
                        (None, None) => {
                            return error(ErrorCode::Unsupported, "a search on the network names a conversation or a Space")
                        }
                    };
                    if !self.shows(&account) {
                        return Response::MessageHits { hits: Vec::new() };
                    }
                    match self.ask(&account, |reply| Command::Search { query, scope, at: std::time::Instant::now(), reply }) {
                        Ok(hits) => Response::MessageHits { hits },
                        Err(refused) => *refused,
                    }
                }
            }
            Request::SetDraft { conversation, text } => match self.hub.store().set_draft(&conversation, &text) {
                Ok(true) => Response::Ok,
                Ok(false) => error(ErrorCode::NotFound, "no such conversation"),
                Err(err) => internal(err),
            },
            Request::Spaces { network } => match self.hub.store().spaces(network) {
                Ok(spaces) => {
                    let spaces = spaces.into_iter().filter(|space| self.shows(&space.key.account())).collect();
                    Response::Spaces { spaces }
                }
                Err(err) => internal(err),
            },
            Request::Tree { space, offset, limit, peek } => {
                if !self.shows(&space.account()) {
                    return error(ErrorCode::NotFound, "signed out");
                }
                let tree = self.hub.store().tree(&space, offset, limit.min(TREE_LIMIT));
                match tree {
                    Ok(Some(tree)) => {
                        if offset == 0 && !peek {
                            self.command(&space.account(), Command::OpenSpace(space.id.clone()));
                        }
                        Response::Tree {
                            space,
                            groups: tree.groups,
                            conversations: self.rows(tree.conversations),
                            total: tree.total,
                        }
                    }
                    Ok(None) => error(ErrorCode::NotFound, "no such space"),
                    Err(err) => internal(err),
                }
            }
            Request::Conversation { conversation } => {
                if !self.shows(&conversation.account()) {
                    return error(ErrorCode::NotFound, "signed out");
                }
                match self.hub.store().conversation(&conversation) {
                    Ok(Some(conversation)) => Response::Conversation { conversation: self.row(conversation) },
                    Ok(None) => error(ErrorCode::NotFound, "no such conversation"),
                    Err(err) => internal(err),
                }
            }
            Request::Collapse { space, group, collapsed } => {
                match self.hub.store().set_collapsed(&space, &group, collapsed) {
                    Ok(true) => Response::Ok,
                    Ok(false) => error(ErrorCode::NotFound, "no such group"),
                    Err(err) => internal(err),
                }
            }
            Request::Star { conversation, starred } => match self.hub.store().set_starred(&conversation, starred) {
                Ok(changed) => {
                    if changed {
                        self.hub.changed(Change::Conversation { conversation });
                    }
                    Response::Ok
                }
                Err(err) => internal(err),
            },

            Request::People { network, relation, query, offset, limit } => {
                self.people(network, relation, query, offset, limit)
            }
            Request::Members { conversation, offset, limit } => {
                let conversation_id = conversation.id.clone();
                let limit = limit.min(100);
                let asked = self.ask(&conversation.account(), |reply| Command::Members {
                    conversation_id,
                    offset,
                    limit,
                    reply,
                });
                match asked {
                    Ok(found) => Response::Members {
                        pending: found.pending,
                        conversation,
                        sections: found.sections,
                        offset,
                        members: found.members,
                    },
                    Err(refused) => *refused,
                }
            }
            Request::StartDirect { person } => {
                let person_id = person.id.clone();
                let started = self.ask(&person.account(), |reply| Command::StartDirect { person_id, reply });
                let id = match started {
                    Ok(id) => id,
                    Err(refused) => return *refused,
                };
                match self.hub.store().conversation(&Key::new(&person.account(), id)) {
                    Ok(Some(conversation)) => Response::Conversation { conversation: self.row(conversation) },
                    Ok(None) => error(ErrorCode::Internal, "started, but not recorded"),
                    Err(err) => internal(err),
                }
            }
            Request::Profile { person, space } => self.profile(person, space),
            Request::ConversationInfo { conversation } => self.conversation_info(conversation),
            Request::SpaceInfo { space } => self.space_info(space),
            Request::SetMute { conversation, notify } => {
                self.act(&conversation, |conversation_id, reply| Command::SetMute { conversation_id, notify, reply })
            }
            Request::Leave { conversation } => self.act(&conversation, |conversation_id, reply| Command::Leave { conversation_id, reply }),
            Request::CloseDirect { conversation } => {
                self.act(&conversation, |conversation_id, reply| Command::CloseDirect { conversation_id, reply })
            }
            Request::Pinned { conversation } => self.pinned(conversation),
            Request::Shared { conversation, kind, before, limit, search } => self.shared(conversation, kind, before, limit, search),
            Request::Relate { person, action } => self.relate(person, action),
            Request::SetPresence { account, presence, status } => self.set_presence(account, presence, status),
            Request::Watch(watching) => self.watch(watching),

            Request::Messages { conversation, page, limit } => self.messages(conversation, page, limit),
            Request::MarkRead { conversation, message_id } => self.mark_read(conversation, message_id),
            Request::Send { conversation, client_id, text, reply_to, with } => {
                let message = self.new_message(&conversation, client_id, text, reply_to.as_deref(), &with);
                self.send(conversation, message, reply_to, with)
            }
            Request::CancelSend { conversation, client_id } => {
                if let Err(refused) = self.unsent(&conversation, &client_id) {
                    return *refused;
                }
                let conversation_id = conversation.id.clone();
                self.command(&conversation.account(), Command::CancelSend { conversation_id, client_id });
                Response::Ok
            }
            Request::Retry { conversation, client_id } => match self.unsent(&conversation, &client_id) {
                Ok(message) => {
                    let reply_to = message.reply_to.as_ref().map(|quoted| quoted.id.clone());
                    // The files go again from where they were; the options were the first try's.
                    let with = Outgoing { files: outgoing_files(&message), ..Outgoing::default() };
                    self.send(conversation, message, reply_to, with)
                }
                Err(refused) => *refused,
            },
            Request::Discard { conversation, client_id } => {
                let message = match self.unsent(&conversation, &client_id) {
                    Ok(message) => message,
                    Err(refused) => return *refused,
                };
                match self.hub.store().delete_message(&conversation, &client_id) {
                    Ok(_) => {
                        // The shell's staged copies in `outbox/` go with it; the user's own stay.
                        crate::blobs::upload::discard(&outgoing_files(&message));
                        self.hub.changed(Change::Messages { conversation });
                        Response::Ok
                    }
                    Err(err) => internal(err),
                }
            }
            Request::Typing { conversation } => {
                let account = conversation.account();
                // P1: off, the network never hears it.
                if self.hub.link(&account) == Link::Connected && self.hub.settings().typing(&account) {
                    self.command(&account, Command::Typing(conversation.id));
                }
                Response::Ok
            }

            Request::React { conversation, message_id, emoji, on } => self.react(conversation, message_id, emoji, on),
            Request::Reactions { conversation } => self.reactions(conversation),
            Request::Edit { conversation, message_id, text } => self.rewrite(conversation, message_id, Some(text)),
            Request::Delete { conversation, message_id } => self.rewrite(conversation, message_id, None),

            Request::Fetch { account, ids, size, retry } => {
                let adapter = lock(&self.adapters).get(&account).map(Adapter::sender);
                let ids = &ids[..ids.len().min(PAGE_LIMIT as usize)];
                // What the cache is asked for is decided here: a failure the user wants tried
                // again is forgotten, and a picture wanted larger than the one held is pointed
                // at a larger registered size, which makes it not fetched yet.
                let prepared = {
                    let mut store = self.hub.store();
                    let retried = if retry { store.media_retry(&account, ids).map(drop) } else { Ok(()) };
                    retried.and_then(|()| match size {
                        Some(size) => store.media_sized(&account, ids, size.min(LARGEST)).map(drop),
                        None => Ok(()),
                    })
                };
                if let Err(err) = prepared {
                    return internal(err);
                }
                match self.blobs.request(&account, ids, adapter.clone()) {
                    Ok(files) => {
                        // What the cache has never heard of may be the adapter's to fetch itself.
                        let unknown: Vec<String> = ids.iter().filter(|id| !files.iter().any(|file| file.id == **id)).cloned().collect();
                        if let (false, Some(adapter)) = (unknown.is_empty(), adapter) {
                            let _ = adapter.send(Command::Wanted(unknown));
                        }
                        Response::Media { files }
                    }
                    Err(err) => internal(err),
                }
            }

            Request::SignIn { network, at } => self.sign_in(network, at),
            Request::SignInBegin { network } => self.sign_in_step(network, None),
            Request::SignInAnswer { network, input } => self.sign_in_step(network, Some(input)),
            Request::SignInCancel { network } => {
                match network {
                    Network::Telegram => adapters::telegram::signin::cancel(),
                    Network::Signal => adapters::signal::signin::cancel(),
                    Network::Messages => adapters::messages::signin::cancel(),
                    Network::Tiny => adapters::tiny::signin::cancel(),
                    _ => {}
                }
                Response::Ok
            }
            Request::SignOut { account } => self.sign_out(account),
            Request::TinyCommunity { account } => adapters::tiny::community(account),
            Request::TinyInvite { account, role } => adapters::tiny::invite(account, role),
            Request::TinyRole { account, user_id, role } => adapters::tiny::set_role(account, user_id, role),
            Request::TinyRemove { account, user_id } => adapters::tiny::remove(account, user_id),

            Request::CallStart { conversation } => self.call_start(conversation),
            Request::CallAccept => match self.hub.calls.accept(&self.hub) {
                Ok(conversation) => {
                    self.join(conversation);
                    self.status()
                }
                Err(err) => call_error(err),
            },
            // Declining, cancelling and leaving are the same on our side: whatever is in the
            // slot goes, and the network is told.
            Request::CallDecline | Request::CallEnd => {
                if let Some(conversation) = self.hub.calls.end(&self.hub) {
                    self.command(&conversation.account(), Command::CallLeave(conversation.id));
                }
                self.status()
            }
            Request::CallSet { muted, deafened } => match self.hub.calls.set_audio(&self.hub, muted, deafened) {
                Ok(conversation) => {
                    self.command(&conversation.account(), Command::CallAudio { muted, deafened });
                    self.status()
                }
                Err(err) => call_error(err),
            },
            Request::Handoff { conversation } => {
                let conversation_id = conversation.id.clone();
                match self.ask(&conversation.account(), |reply| Command::Handoff { conversation_id, reply }) {
                    Ok(url) => Response::Link { url },
                    Err(refused) => *refused,
                }
            }
            Request::Play(request) => match self.hub.player.request(&self.hub, &support_dir().join("media"), request) {
                Ok(()) => Response::Ok,
                Err(detail) => error(ErrorCode::Media, detail),
            },
            Request::Media(request) => {
                let asked = match request {
                    // Joining is the adapter's to ask for, with what the network handed out.
                    MediaRequest::Join(_) | MediaRequest::Rejoin(_) => {
                        return error(ErrorCode::Unsupported, "a call is joined with call_start")
                    }
                    MediaRequest::Meter { on } => self.hub.calls.meter(&self.hub, on),
                    request => self.hub.calls.request(&self.hub, &request).map(drop),
                };
                match asked {
                    Ok(()) => self.status(),
                    Err(err) => call_error(err),
                }
            }

            Request::Settings => match self.hub.store().settings() {
                Ok(values) => Response::Settings { values },
                Err(err) => internal(err),
            },
            Request::Set { scope, key, value } => self.set(scope, key, value),
            Request::ResetSettings => self.reset_settings(),
            Request::Shutdown => self.shutdown(),
            Request::Storage => self.storage(),
            Request::Clear => self.clear(),
            Request::Prunable { days } => match self.hub.store().prunable(now(), days) {
                Ok((messages, bytes)) => Response::Prunable { messages, bytes },
                Err(err) => internal(err),
            },
            Request::TestMicrophone { on } => match self.hub.calls.test_microphone(&self.hub, on) {
                Ok(()) => Response::Ok,
                Err(err) => call_error(err),
            },
            Request::TestSound => match self.hub.calls.test_sound(&self.hub) {
                Ok(()) => Response::Ok,
                Err(err) => call_error(err),
            },
        }
    }
}

/// Keeps the session a sign-in drawn by the shell ended with and starts its accounts, as
/// `Chat::sign_in` does with a helper's. Answers the accounts it started.
fn adopt(hub: &Arc<Hub>, adapters: &Adapters, network: Network, session: &str) -> Result<Vec<AccountKey>, String> {
    // A scratch directory keeps no session. The account still starts there, for a test.
    if session::enabled() {
        session::save(network, session)?;
    }
    lock(adapters).retain(|account, _| account.network != network);
    let mut started = Vec::new();
    for signed in adapters::accounts(network, session)? {
        let account = AccountKey {
            network,
            account: signed.account,
        };
        hub.store().set_account(&account, &signed.name, "").map_err(|err| err.to_string())?;
        let adapter = adapters::start(account.clone(), &signed.session, hub.clone())?;
        lock(adapters).insert(account.clone(), adapter);
        started.push(account);
    }
    hub.changed(Change::Status);
    Ok(started)
}

/// One connection's requests, in turn. A frame that does not parse (a shell of another
/// version) and an answer too large for a frame are answered in words, not by hanging up. A
/// request that panics is answered `Internal` and the connection goes on: the panic is logged
/// by the hook, and the locks it poisoned are taken anyway (`hub::lock`).
fn serve_connection(chat: &Chat, mut stream: ChatStream) -> io::Result<()> {
    loop {
        let bytes = match read_frame_bytes(&mut stream) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        };
        let answer = match serde_json::from_slice::<Request>(&bytes) {
            Ok(request) => {
                let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| chat.handle(request)));
                handled.unwrap_or_else(|_| error(ErrorCode::Internal, "tinyclient hit a problem answering this. It has been noted."))
            }
            Err(err) => error(ErrorCode::Unsupported, format!("chatd does not understand this request; tinyclient and chatd may be different versions ({err})")),
        };
        match write_frame(&mut stream, &answer) {
            Err(err) if err.kind() == ErrorKind::InvalidData => {
                write_frame(&mut stream, &error(ErrorCode::TooLarge, "The answer was too large. Ask for a smaller page."))?
            }
            written => written?,
        }
    }
}

/// Answers requests until the process is killed. A connection may send many requests, and a
/// slow one (a sign-in, a wait) holds up only its own connection. The sessions are read once
/// the socket is answering (`Chat::read_sessions`). A failed accept or thread spawn (out of
/// file descriptors) drops that one connection and goes on.
pub fn serve(listener: ChatListener, chat: Chat) -> io::Result<()> {
    let chat = Arc::new(chat);
    let _ = chat.this.set(Arc::downgrade(&chat));
    Chat::read_sessions(&chat, Network::ALL.to_vec(), true);
    loop {
        let stream = match listener.accept() {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("chatd: accept: {err}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let chat = chat.clone();
        let spawned = std::thread::Builder::new().name("connection".into()).stack_size(256 * 1024).spawn(move || {
            if let Err(err) = serve_connection(&chat, stream) {
                eprintln!("chatd: connection: {err}");
            }
        });
        if let Err(err) = spawned {
            eprintln!("chatd: connection thread: {err}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// The name a file is sent under.
fn file_name(file: &protocol::OutgoingFile) -> String {
    if !file.name.is_empty() {
        return file.name.clone();
    }
    std::path::Path::new(&file.path).file_name().map_or_else(|| file.path.clone(), |name| name.to_string_lossy().into_owned())
}

/// What a file to be sent is, from its name: enough for the pending row to draw it.
fn kind_of(path: &str) -> MediaKind {
    let extension = std::path::Path::new(path).extension().map(|extension| extension.to_string_lossy().to_lowercase()).unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "webp" => MediaKind::Image,
        "gif" => MediaKind::Gif,
        "mp4" | "mov" | "m4v" | "webm" => MediaKind::Video,
        "mp3" | "m4a" | "ogg" | "opus" | "wav" => MediaKind::Audio,
        _ => MediaKind::File,
    }
}

fn megabytes(bytes: u64) -> String {
    match bytes {
        bytes if bytes >= 1 << 30 => format!("{} GB", bytes >> 30),
        bytes => format!("{} MB", bytes >> 20),
    }
}

/// The files of an unsent message, to send again from where they were.
fn outgoing_files(message: &Message) -> Vec<protocol::OutgoingFile> {
    let ours = message.attachments.iter().filter(|attachment| attachment.file.id.starts_with("upload-"));
    ours.filter_map(|attachment| {
        Some(protocol::OutgoingFile {
            path: attachment.file.path.clone()?,
            name: attachment.name.clone(),
            spoiler: attachment.spoiler,
            as_file: false,
            original: false,
        })
    })
    .collect()
}

/// The card as the shell gets it: each value in its typed place, cut to its length. A line an
/// adapter still gives as a labelled `field` moves to its place when the label names one.
fn typed_profile(person: Person, mut about: About) -> Profile {
    let mut leftovers = Vec::new();
    for (label, value) in std::mem::take(&mut about.fields) {
        if label.is_empty() || value.is_empty() {
            continue;
        }
        let empty = |held: &String| held.is_empty();
        match label.to_lowercase().as_str() {
            "pronouns" if empty(&about.pronouns) => about.pronouns = value,
            "title" | "job title" if empty(&about.title) => about.title = value,
            "department" if empty(&about.department) => about.department = value,
            "nickname" if empty(&about.nickname) => about.nickname = value,
            "email" | "e-mail" => about.contacts.push(protocol::Contact { kind: protocol::ContactKind::Email, value, masked: false }),
            "phone" | "mobile" => about.contacts.push(protocol::Contact { kind: protocol::ContactKind::Phone, value, masked: true }),
            "roles" if about.roles.is_empty() => about.roles = value.split(", ").map(str::to_owned).collect(),
            // The handle is on the person already.
            "username" | "handle" if value.trim_start_matches('@') == person.handle.trim_start_matches('@') => {}
            _ => leftovers.push((label, value)),
        }
    }
    leftovers.truncate(protocol::PROFILE_FIELDS);
    for (label, value) in &mut leftovers {
        clip(label, 40);
        clip(value, 200);
    }
    for text in [&mut about.nickname, &mut about.pronouns, &mut about.title, &mut about.department, &mut about.away] {
        clip(text, 200);
    }
    clip(&mut about.bio, 1_000);
    clip(&mut about.note, 1_000);
    clip(&mut about.time_zone, 64);
    about.mutual_spaces.truncate(MUTUAL_SPACES);
    about.mutual_spaces.iter_mut().for_each(|title| clip(title, 100));
    about.roles.retain(|role| !role.is_empty());
    about.roles.truncate(protocol::PROFILE_ROLES);
    about.roles.iter_mut().for_each(|role| clip(role, 100));
    about.contacts.truncate(protocol::PROFILE_CONTACTS);
    for contact in &mut about.contacts {
        clip(&mut contact.value, 200);
        // Phone numbers are never drawn in full until clicked, whoever the network is.
        contact.masked |= contact.kind == protocol::ContactKind::Phone;
    }
    let mut profile = Profile::of(person);
    profile.bio = about.bio;
    profile.mutual_spaces = about.mutual_spaces;
    profile.time_zone = about.time_zone;
    profile.utc_offset = about.utc_offset;
    profile.fields = leftovers;
    profile.since = about.since;
    profile.mutual_friends = about.mutual_friends;
    profile.accent = about.accent;
    profile.banner = about.banner;
    profile.nickname = about.nickname;
    profile.pronouns = about.pronouns;
    profile.title = about.title;
    profile.department = about.department;
    profile.roles = about.roles;
    profile.contacts = about.contacts;
    profile.mutual_total = about.mutual_total;
    profile.joined = about.joined;
    profile.friends_since = about.friends_since;
    profile.last_seen = about.last_seen;
    profile.status_until = about.status_until;
    profile.away = about.away;
    profile.away_until = about.away_until;
    profile.note = about.note;
    profile.warning = about.warning;
    profile
}
