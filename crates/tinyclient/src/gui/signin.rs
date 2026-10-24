//! The Telegram, Signal, Messages, and Tiny server sign-in sheet (`docs/design/08-first-run-settings.md`, section 2).
//! None of them has a web page to show, so tinyclient draws the sign-in itself. Telegram: a code to
//! scan, or a phone number, the code Telegram sends and, for accounts that have one, the two-step
//! password. Signal: only the code to scan from the phone, which links this computer as one more
//! device; a number typed here would take the account from the phone, so none is asked. A Tiny
//! server asks for its address, then an email, then a password of 12 to 128 characters.
//! A code from someone already on that server is its own step, then the name people there will see.
//!
//! chatd owns the sign-in. The sheet asks it to begin, draws the step it is told
//! (`protocol::SignInStep`) and answers it; while the sheet is open its own loop asks `Wait` and
//! picks out `Change::SignIn`, because a step also moves by itself: the code is replaced when it
//! expires, and scanning it ends the sign-in from the phone. Every socket call is off the main
//! thread.
//!
//! The shell knows two things about this file: `open`, which `Shell::sign_in` tries first, and
//! `layer`, which the root draws over everything. The state is an entity in a GPUI global.
//!
//! What is typed (a code, a password) is held only by its field, is cleared from it the moment
//! it is sent, and is never logged.

mod qr;
#[cfg(feature = "gui-shot")]
mod shot;

use super::components::{button, button_disabled, fade_in, field, glyph, network_icon, scrim, ButtonKind};
use super::input::{InputEvent, InputKey, TextInput};
use super::state::Shell;
use super::theme::{self, Type};
use super::time::clock;
use crate::client::ask;
use gpui::{
    canvas, deferred, div, fill, point, prelude::*, px, rgb, size, App, Bounds, Context, Deferred, Div, Entity,
    FocusHandle, Focusable, Global, MouseButton, Rgba, SharedString, Stateful, Task, Window,
};
use protocol::{Change, CodeVia, ErrorCode, Network, Request, Response, SignInInput, SignInProblem, SignInStep};
use std::io;
use std::rc::Rc;
use std::time::Duration;

/// The QR tile only: a phone reads dark on white, whatever the theme around it.
const QR_PAPER: u32 = 0xffffff;
const QR_INK: u32 = 0x0c0d11;
/// The tile, the quiet zone inside it, and so the room the code is drawn in.
const TILE: f32 = 232.;
const TILE_PAD: f32 = 16.;
const TILE_RADIUS: f32 = 14.;
const CODE_ROOM: f32 = TILE - 2. * TILE_PAD;
/// A text field of the sheet.
const FIELD: f32 = 40.;
/// One box of the code.
const BOX: (f32, f32) = (48., 56.);
/// How long to wait before asking chatd again when it did not answer.
const RETRY: Duration = Duration::from_secs(2);
/// Above the popovers and menus of the shell, which use 1 to 3.
const PRIORITY: usize = 8;

/// What the sheet shows.
#[derive(Clone, PartialEq, Eq)]
enum Face {
    Step(SignInStep),
    /// This build cannot sign in to the network at all. chatd's words; only "Close".
    Unsupported(String),
}

/// The sheet's state: an entity of its own, so the shell holds nothing for it.
pub struct Sheet {
    network: Network,
    /// `None`: closed, and nothing is drawn.
    face: Option<Face>,
    /// An answer is on its way to chatd: the primary button is off and nothing else is sent.
    busy: bool,
    /// A problem found here, before or instead of chatd's: a number too short to send, or
    /// chatd not answering.
    local: Option<SignInProblem>,
    /// Counts opens and restarts, so an answer to an earlier one is dropped.
    epoch: u64,
    /// The code on show and the link it was made from: encoded once, redrawn only on rotation.
    code: Option<(String, Rc<qr::Qr>)>,
    /// Hears `Change::SignIn` while the sheet is open. Dropping it stops the loop.
    listener: Option<Task<()>>,
    /// Fires once, when Telegram allows another code, to swap the link in.
    resend_timer: Option<Task<()>>,
    country: Entity<TextInput>,
    number: Entity<TextInput>,
    /// The one field behind the code's boxes.
    digits: Entity<TextInput>,
    /// The field behind the password's bullets.
    password: Entity<TextInput>,
    /// A Tiny server's address.
    url: Entity<TextInput>,
    /// The email on that server.
    email: Entity<TextInput>,
    /// An invite code for that server.
    invite: Entity<TextInput>,
    /// The name people on that server will see.
    name: Entity<TextInput>,
    /// Has the keys when no field does, so Escape still closes the sheet.
    focus: FocusHandle,
    /// The step changed: put the keys where typing goes, on the next frame.
    refocus: bool,
    /// What had the keys before the sheet opened; it gets them back when it closes.
    before: Option<FocusHandle>,
    /// `TINYCLIENT_SIGNIN` has been looked at.
    launched: bool,
    /// A made-up step for a screenshot: chatd is never asked.
    canned: bool,
}

struct Holder(Entity<Sheet>);

impl Global for Holder {}

fn holder(cx: &mut App) -> Entity<Sheet> {
    if let Some(holder) = cx.try_global::<Holder>() {
        return holder.0.clone();
    }
    let sheet = cx.new(Sheet::new);
    cx.set_global(Holder(sheet.clone()));
    sheet
}

/// Opens the sheet for a network that signs in here. `false` for one that signs in through its
/// own web page, so the caller goes on to the helper window.
pub fn open(network: Network, cx: &mut Context<Shell>) -> bool {
    if !matches!(network, Network::Telegram | Network::Signal | Network::Messages | Network::Tiny) {
        return false;
    }
    holder(cx).update(cx, |sheet, cx| sheet.open(network, cx));
    cx.notify();
    true
}

/// The sheet, when it is open: drawn after everything else in the window, so it covers the
/// first-run screen and the full shell alike.
pub fn layer(window: &mut Window, cx: &mut App) -> Option<Deferred> {
    let sheet = holder(cx);
    let (open, restore) = sheet.update(cx, |sheet, cx| {
        if !std::mem::replace(&mut sheet.launched, true) {
            sheet.launch(cx);
        }
        let open = sheet.face.is_some();
        (open, if open { None } else { sheet.before.take() })
    });
    if let Some(before) = restore {
        window.defer(cx, move |window, cx| window.focus(&before, cx));
    }
    open.then(|| deferred(sheet).with_priority(PRIORITY))
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs() as i64)
}

/// The words for a problem, naming the network that is signing in.
fn problem_words(network: Network, problem: &SignInProblem) -> String {
    let name = network.label();
    let tiny = network == Network::Tiny;
    match problem {
        SignInProblem::Unreachable if tiny => "Couldn't reach that server. Check the address and your connection.".to_owned(),
        SignInProblem::Unreachable => format!("Couldn't reach {name}. Check your connection."),
        SignInProblem::PhoneInvalid => "That doesn't look like a phone number.".to_owned(),
        SignInProblem::PhoneUnknown => format!("No {name} account uses this number. Create one in the {name} app first."),
        SignInProblem::PhoneBanned => format!("{name} has blocked this number."),
        SignInProblem::CodeInvalid => "That code isn't right. Check it and type it again.".to_owned(),
        SignInProblem::CodeExpired => "That code has expired.".to_owned(),
        SignInProblem::PasswordInvalid if tiny => "That email or password isn't right.".to_owned(),
        SignInProblem::PasswordInvalid => "That password isn't right.".to_owned(),
        SignInProblem::UrlInvalid => "Type a host, like tiny.example.com.".to_owned(),
        SignInProblem::EmailInvalid => "Type an email address, like name@example.com.".to_owned(),
        SignInProblem::TooMany { .. } if tiny => "Too many tries. Wait a minute, then try again.".to_owned(),
        SignInProblem::TooMany { until } => format!("Too many tries. {name} allows another at {}.", clock(*until)),
        SignInProblem::Other { reason } => reason.clone(),
    }
}

/// The sentence under "Enter the code": how the code was sent, and for which number.
fn sent_words(via: CodeVia, length: u8, phone: &str) -> String {
    let phone = phone.trim();
    let code = format!("a {length}-digit code");
    match (via, phone.is_empty()) {
        (CodeVia::App, true) => format!("Telegram sent {code} to the Telegram app on your other devices."),
        (CodeVia::App, false) => format!("Telegram sent {code} to the Telegram app on your other devices, for {phone}."),
        (CodeVia::Sms, true) => format!("Telegram sent {code} by SMS."),
        (CodeVia::Sms, false) => format!("Telegram sent {code} by SMS to {phone}."),
        (CodeVia::Call, true) => format!("Telegram is calling you to read out {code}."),
        (CodeVia::Call, false) => format!("Telegram is calling {phone} to read out {code}."),
        (CodeVia::Email, true) => format!("Telegram sent {code} to the email address on your account."),
        (CodeVia::Email, false) => format!("Telegram sent {code} to the email address on your account, for {phone}."),
        (CodeVia::Other, true) => format!("Telegram sent {code}."),
        (CodeVia::Other, false) => format!("Telegram sent {code} for {phone}."),
    }
}

/// What the link that sends the code again says, when Telegram offers another way.
fn resend_words(next: Option<CodeVia>) -> Option<&'static str> {
    match next? {
        CodeVia::Sms => Some("Send the code by SMS"),
        _ => Some("Send a new code"),
    }
}

/// The number as chatd wants it: `+` and digits only. `None` when there are fewer than seven,
/// which no country's numbers are.
fn phone_number(country: &str, number: &str) -> Option<String> {
    let digits: String = country.chars().chain(number.chars()).filter(char::is_ascii_digit).collect();
    (digits.len() >= 7).then(|| format!("+{digits}"))
}

/// The number as the user reads it: the country code, then the rest as they spaced it.
fn phone_shown(country: &str, number: &str) -> String {
    let code: String = country.chars().filter(char::is_ascii_digit).collect();
    let rest = number.trim().trim_start_matches('+');
    if code.is_empty() { format!("+{rest}") } else { format!("+{code} {rest}") }
}

/// What the boxes keep of what was typed or pasted: the digits, as many as there are boxes.
fn code_digits(typed: &str, length: usize) -> String {
    typed.chars().filter(char::is_ascii_digit).take(length).collect()
}

/// How the code is drawn in `room` points: whole points per module, and the margin that
/// centers it. A module size with a fraction would blur every edge.
fn module_size(modules: usize, room: f32) -> (f32, f32) {
    let module = (room / modules.max(1) as f32).floor().max(1.);
    (module, ((room - module * modules as f32) / 2.).floor().max(0.))
}

fn step_problem(step: &SignInStep) -> Option<&SignInProblem> {
    match step {
        SignInStep::Qr { problem, .. }
        | SignInStep::Phone { problem }
        | SignInStep::Code { problem, .. }
        | SignInStep::Password { problem, .. }
        | SignInStep::Server { problem }
        | SignInStep::Email { problem, .. }
        | SignInStep::Invite { problem, .. }
        | SignInStep::Name { problem, .. } => problem.as_ref(),
        SignInStep::Done { .. } | SignInStep::Failed { .. } => None,
    }
}

fn waiting() -> SignInStep {
    SignInStep::Qr { link: String::new(), expires: 0, problem: None }
}

/// What the sheet draws before chatd has answered. Tiny starts on the address, not a code.
fn waiting_for(network: Network) -> SignInStep {
    if network == Network::Tiny {
        SignInStep::Server { problem: None }
    } else {
        waiting()
    }
}

/// `TINYCLIENT_SIGNIN` names a sheet. Tiny's label is "Tiny server"; `tiny` opens it too.
fn asked_network(word: &str) -> Option<Network> {
    [Network::Telegram, Network::Signal, Network::Messages, Network::Tiny]
        .into_iter()
        .find(|network| network.label().eq_ignore_ascii_case(word) || (*network == Network::Tiny && word.eq_ignore_ascii_case("tiny")))
}

fn failed(reason: &str) -> Face {
    Face::Step(SignInStep::Failed { reason: reason.to_owned() })
}

const NO_CHATD: &str = "tinyclient's background service isn't answering. Try again in a moment.";

impl Sheet {
    fn new(cx: &mut Context<Sheet>) -> Sheet {
        let country = cx.new(|cx| TextInput::new("+1", cx));
        let number = cx.new(|cx| TextInput::new("Phone number", cx));
        let digits = cx.new(|cx| TextInput::new("", cx));
        let password = cx.new(|cx| TextInput::new("", cx));
        let url = cx.new(|cx| TextInput::new("tiny.example.com", cx));
        let email = cx.new(|cx| TextInput::new("Email", cx));
        let invite = cx.new(|cx| TextInput::new("Invite code", cx));
        let name = cx.new(|cx| TextInput::new("Name", cx));
        for input in [&country, &number] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| match event {
                InputEvent::Submit => this.send_phone(cx),
                InputEvent::Changed => {
                    this.dismiss();
                    cx.notify();
                }
            })
            .detach();
        }
        cx.subscribe(&digits, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Changed) {
                this.code_typed(cx);
            }
        })
        .detach();
        cx.subscribe(&password, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Submit => this.send_password(cx),
            InputEvent::Changed => {
                this.dismiss();
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&url, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Submit => this.send_url(cx),
            InputEvent::Changed => {
                this.dismiss();
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&email, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Submit => this.send_email(cx),
            InputEvent::Changed => {
                this.dismiss();
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&invite, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Submit => this.send_invite(cx),
            InputEvent::Changed => {
                this.dismiss();
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&name, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Submit => this.send_name(cx),
            InputEvent::Changed => {
                this.dismiss();
                cx.notify();
            }
        })
        .detach();
        // Escape in a field is the field's key; it hands it on.
        for input in [&country, &number, &digits, &password, &url, &email, &invite, &name] {
            cx.subscribe(input, |this, _, event: &InputKey, cx| {
                if matches!(event, InputKey::Escape) {
                    this.close(true, cx);
                }
            })
            .detach();
        }
        Sheet {
            network: Network::Telegram,
            face: None,
            busy: false,
            local: None,
            epoch: 0,
            code: None,
            listener: None,
            resend_timer: None,
            country,
            number,
            digits,
            password,
            url,
            email,
            invite,
            name,
            focus: cx.focus_handle(),
            refocus: false,
            before: None,
            launched: false,
            canned: false,
        }
    }

    /// Once, at the first frame: `TINYCLIENT_SIGNIN=telegram` (or `signal`, `messages`, `tiny`) opens
    /// the sheet, which is how it is reached while the first-run row still says "Not available yet".
    /// A screenshot build may ask for a made-up step instead.
    fn launch(&mut self, cx: &mut Context<Sheet>) {
        #[cfg(feature = "gui-shot")]
        if let Some(canned) = shot::canned() {
            self.show_canned(canned, cx);
            return;
        }
        let asked = std::env::var("TINYCLIENT_SIGNIN").ok().as_deref().and_then(asked_network);
        if let Some(network) = asked {
            self.open(network, cx);
        }
    }

    #[cfg(feature = "gui-shot")]
    fn show_canned(&mut self, canned: shot::Canned, cx: &mut Context<Sheet>) {
        self.canned = true;
        self.clear_fields(cx);
        match canned.face {
            Face::Step(step) => self.show(step, cx),
            face => self.face = Some(face),
        }
        // After the step, which clears what was typed for the one before it.
        self.number.update(cx, |input, cx| input.set_text(canned.number, cx));
        self.digits.update(cx, |input, cx| input.set_text(canned.digits, cx));
        self.password.update(cx, |input, cx| input.set_text(canned.password, cx));
        self.local = canned.local;
        self.refocus = true;
        cx.notify();
    }

    fn open(&mut self, network: Network, cx: &mut Context<Sheet>) {
        if self.face.is_some() {
            return;
        }
        self.network = network;
        self.clear_fields(cx);
        // The tile is up at once, saying a code is on its way.
        self.face = Some(Face::Step(waiting_for(network)));
        self.refocus = true;
        self.begin(cx);
        cx.notify();
    }

    /// Closes the sheet. `cancel` tells chatd to drop the sign-in; after `Done` there is none.
    fn close(&mut self, cancel: bool, cx: &mut Context<Sheet>) {
        if self.face.take().is_none() {
            return;
        }
        self.epoch += 1;
        self.listener = None;
        self.resend_timer = None;
        self.busy = false;
        self.local = None;
        self.code = None;
        self.clear_fields(cx);
        if cancel && !self.canned {
            let request = Request::SignInCancel { network: self.network };
            cx.background_executor().spawn(async move { drop(ask(&request)) }).detach();
        }
        cx.notify();
    }

    fn clear_fields(&mut self, cx: &mut Context<Sheet>) {
        self.clear_typed(cx);
        self.number.update(cx, |input, cx| input.set_text("", cx));
        self.country.update(cx, |input, cx| input.set_text("+1", cx));
        self.url.update(cx, |input, cx| input.set_text("", cx));
        self.email.update(cx, |input, cx| input.set_text("", cx));
        self.invite.update(cx, |input, cx| input.set_text("", cx));
        self.name.update(cx, |input, cx| input.set_text("", cx));
    }

    /// Drops a typed code and password. They live nowhere else.
    fn clear_typed(&mut self, cx: &mut Context<Sheet>) {
        self.digits.update(cx, |input, cx| input.set_text("", cx));
        self.password.update(cx, |input, cx| input.set_text("", cx));
    }

    /// The red line describes the text that was submitted. Editing that text takes the line away.
    fn dismiss(&mut self) {
        self.local = None;
        let Some(Face::Step(step)) = &mut self.face else {
            return;
        };
        match step {
            SignInStep::Phone { problem }
            | SignInStep::Server { problem }
            | SignInStep::Email { problem, .. }
            | SignInStep::Invite { problem, .. }
            | SignInStep::Name { problem, .. }
            | SignInStep::Password { problem, .. } => *problem = None,
            _ => {}
        }
    }

    // Talking to chatd.

    /// Begins the sign-in, or reads where it stands, then listens for the step moving by itself.
    /// The place to listen from is read before beginning, so no step in between is missed.
    fn begin(&mut self, cx: &mut Context<Sheet>) {
        if self.canned {
            return;
        }
        self.epoch += 1;
        self.busy = false;
        let (epoch, network) = (self.epoch, self.network);
        self.listener = Some(cx.spawn(async move |this, cx| {
            let background = cx.background_executor().clone();
            let seq = || async {
                match background.spawn(async { ask(&Request::Status) }).await {
                    Ok(Response::Status { seq, .. }) => Some(seq),
                    _ => None,
                }
            };
            let begin = || background.spawn(async move { ask(&Request::SignInBegin { network }) });
            let mut since = seq().await;
            let begun = begin().await;
            if !this.update(cx, |this, cx| this.begun(epoch, begun, cx)).unwrap_or(false) {
                return;
            }
            loop {
                let Some(from) = since else {
                    background.timer(RETRY).await;
                    since = seq().await;
                    continue;
                };
                let Ok(Response::Changes { seq, reset, changes }) = background.spawn(async move { ask(&Request::Wait { since: from }) }).await else {
                    since = None;
                    continue;
                };
                since = Some(seq);
                let steps: Vec<SignInStep> = changes
                    .into_iter()
                    .filter_map(|change| match change {
                        Change::SignIn { network: theirs, step } if theirs == network => Some(step),
                        _ => None,
                    })
                    .collect();
                let heard = this.update(cx, |this, cx| {
                    if this.epoch != epoch || this.face.is_none() {
                        return false;
                    }
                    for step in steps {
                        this.show(step, cx);
                    }
                    this.face.is_some()
                });
                if !heard.unwrap_or(false) {
                    return;
                }
                // chatd lost track of what changed: read the step again.
                if reset {
                    let again = begin().await;
                    if !this.update(cx, |this, cx| this.begun(epoch, again, cx)).unwrap_or(false) {
                        return;
                    }
                }
            }
        }));
    }

    /// chatd's answer to beginning. `false` when there is nothing to listen for.
    fn begun(&mut self, epoch: u64, answer: io::Result<Response>, cx: &mut Context<Sheet>) -> bool {
        if epoch != self.epoch || self.face.is_none() {
            return false;
        }
        cx.notify();
        match answer {
            Ok(Response::SignIn { step, .. }) => {
                self.show(step, cx);
                self.face.is_some()
            }
            Ok(Response::Error { code: ErrorCode::Unsupported, detail }) => {
                let words = if detail.is_empty() { format!("This build of tinyclient can't sign in to {}.", self.network.label()) } else { detail };
                self.face = Some(Face::Unsupported(words));
                false
            }
            Ok(Response::Error { detail, .. }) if !detail.is_empty() => {
                self.face = Some(failed(&detail));
                false
            }
            Ok(_) => {
                self.face = Some(failed(crate::client::OTHER_ANSWER));
                false
            }
            Err(_) => {
                self.face = Some(failed(NO_CHATD));
                false
            }
        }
    }

    /// Sends what the user did about the step on show and draws the step chatd answers with.
    fn answer(&mut self, input: SignInInput, cx: &mut Context<Sheet>) {
        if self.busy || self.canned {
            return;
        }
        self.busy = true;
        self.local = None;
        let epoch = self.epoch;
        let request = Request::SignInAnswer { network: self.network, input };
        cx.spawn(async move |this, cx| {
            // The request holds what was typed. It is moved into the call and dropped there.
            let answer = cx.background_executor().spawn(async move { ask(&request) }).await;
            let _ = this.update(cx, |this, cx| this.answered(epoch, answer, cx));
        })
        .detach();
        cx.notify();
    }

    fn answered(&mut self, epoch: u64, answer: io::Result<Response>, cx: &mut Context<Sheet>) {
        if epoch != self.epoch || self.face.is_none() {
            return;
        }
        self.busy = false;
        self.refocus = true;
        cx.notify();
        match answer {
            Ok(Response::SignIn { step, .. }) => self.show(step, cx),
            Ok(Response::Error { detail, .. }) if !detail.is_empty() => self.local = Some(SignInProblem::Other { reason: detail }),
            Ok(_) => self.local = Some(SignInProblem::Other { reason: crate::client::OTHER_ANSWER.to_owned() }),
            Err(_) => self.local = Some(SignInProblem::Other { reason: NO_CHATD.to_owned() }),
        }
    }

    /// Draws a step, whoever said it: an answer, or the sign-in moving by itself.
    fn show(&mut self, step: SignInStep, cx: &mut Context<Sheet>) {
        if matches!(step, SignInStep::Done { .. }) {
            // The shell's own loop hears the new account and draws it.
            self.close(false, cx);
            return;
        }
        let moved = !matches!(&self.face, Some(Face::Step(old)) if std::mem::discriminant(old) == std::mem::discriminant(&step));
        if moved {
            self.local = None;
        }
        // A wrong code starts clean. A password stays, so a long one is not typed again.
        if moved || step_problem(&step).is_some() {
            let keep_password = !moved && matches!(step, SignInStep::Password { .. });
            if keep_password {
                self.digits.update(cx, |input, cx| input.set_text("", cx));
            } else {
                self.clear_typed(cx);
            }
            self.refocus = true;
        }
        match &step {
            SignInStep::Qr { link, .. } if link.is_empty() => self.code = None,
            SignInStep::Qr { link, .. } => {
                if self.code.as_ref().is_none_or(|(old, _)| old != link) {
                    self.code = qr::encode(link.as_bytes()).map(|code| (link.clone(), Rc::new(code)));
                }
            }
            _ => self.code = None,
        }
        self.resend_timer = match &step {
            SignInStep::Code { resend_at, .. } if *resend_at > now() => {
                let wait = Duration::from_secs((*resend_at - now()) as u64);
                Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(wait).await;
                    let _ = this.update(cx, |_, cx| cx.notify());
                }))
            }
            _ => None,
        };
        self.face = Some(Face::Step(step));
        cx.notify();
    }

    /// Starts over from nothing: "Try again" where the sign-in cannot go on.
    fn restart(&mut self, cx: &mut Context<Sheet>) {
        if self.canned {
            return;
        }
        self.local = None;
        self.code = None;
        self.clear_typed(cx);
        self.face = Some(Face::Step(waiting_for(self.network)));
        self.refocus = true;
        // Bumped now, so an answer still on its way to the old sign-in is dropped.
        self.epoch += 1;
        self.listener = None;
        let network = self.network;
        let cancel = cx.background_executor().spawn(async move { drop(ask(&Request::SignInCancel { network })) });
        cx.spawn(async move |this, cx| {
            cancel.await;
            let _ = this.update(cx, |this, cx| {
                if this.face.is_some() {
                    this.begin(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    // What the user does.

    fn send_phone(&mut self, cx: &mut Context<Sheet>) {
        if self.busy {
            return;
        }
        let (country, number) = (self.country.read(cx).text().to_owned(), self.number.read(cx).text().to_owned());
        match phone_number(&country, &number) {
            // Sent as it reads, so the next step can say it back the way it was typed; chatd
            // keeps the digits.
            Some(_) => self.answer(SignInInput::Phone { number: phone_shown(&country, &number) }, cx),
            None => {
                self.local = Some(SignInProblem::PhoneInvalid);
                cx.notify();
            }
        }
    }

    /// The field behind the boxes changed. It keeps digits only; the last one sends the code.
    fn code_typed(&mut self, cx: &mut Context<Sheet>) {
        let Some(Face::Step(SignInStep::Code { length, .. })) = &self.face else {
            return;
        };
        let length = usize::from(*length).max(1);
        let typed = self.digits.read(cx).text().to_owned();
        let code = code_digits(&typed, length);
        if self.busy {
            // One code at a time: what is typed while an answer is awaited is not kept.
            self.digits.update(cx, |input, cx| input.set_text("", cx));
        } else if code.len() == length {
            self.digits.update(cx, |input, cx| input.set_text("", cx));
            self.answer(SignInInput::Code { code }, cx);
        } else if code != typed {
            self.digits.update(cx, |input, cx| input.set_text(&code, cx));
        }
        cx.notify();
    }

    fn send_password(&mut self, cx: &mut Context<Sheet>) {
        if self.busy {
            return;
        }
        let password = self.password.read(cx).text().to_owned();
        if password.is_empty() {
            return;
        }
        // Left in the field. A miss would make a long password be typed again.
        self.dismiss();
        self.answer(SignInInput::Password { password }, cx);
    }

    fn send_url(&mut self, cx: &mut Context<Sheet>) {
        if self.busy {
            return;
        }
        let url = self.url.read(cx).text().to_owned();
        if url.trim().is_empty() {
            return;
        }
        self.dismiss();
        self.answer(SignInInput::Server { url }, cx);
    }

    fn send_email(&mut self, cx: &mut Context<Sheet>) {
        if self.busy {
            return;
        }
        let email = self.email.read(cx).text().to_owned();
        if email.trim().is_empty() {
            return;
        }
        self.dismiss();
        self.answer(SignInInput::Email { email }, cx);
    }

    fn send_have_code(&mut self, cx: &mut Context<Sheet>) {
        if self.busy {
            return;
        }
        let url = self.url.read(cx).text().to_owned();
        self.dismiss();
        self.answer(SignInInput::HaveCode { url }, cx);
    }

    fn send_invite(&mut self, cx: &mut Context<Sheet>) {
        if self.busy {
            return;
        }
        let code = self.invite.read(cx).text().to_owned();
        if code.trim().is_empty() {
            return;
        }
        // A code is not kept in the field after it is sent.
        self.invite.update(cx, |input, cx| input.set_text("", cx));
        self.dismiss();
        self.answer(SignInInput::Invite { code }, cx);
    }

    fn send_name(&mut self, cx: &mut Context<Sheet>) {
        if self.busy {
            return;
        }
        let name = self.name.read(cx).text().to_owned();
        if name.trim().is_empty() {
            return;
        }
        self.dismiss();
        self.answer(SignInInput::Name { name }, cx);
    }

    /// Where typing goes on the step on show.
    fn target(&self, cx: &App) -> FocusHandle {
        match &self.face {
            Some(Face::Step(SignInStep::Phone { .. })) => self.number.focus_handle(cx),
            Some(Face::Step(SignInStep::Code { .. })) => self.digits.focus_handle(cx),
            Some(Face::Step(SignInStep::Password { .. })) => self.password.focus_handle(cx),
            Some(Face::Step(SignInStep::Server { .. })) => self.url.focus_handle(cx),
            Some(Face::Step(SignInStep::Email { .. })) => self.email.focus_handle(cx),
            Some(Face::Step(SignInStep::Invite { .. })) => self.invite.focus_handle(cx),
            Some(Face::Step(SignInStep::Name { .. })) => self.name.focus_handle(cx),
            _ => self.focus.clone(),
        }
    }

    /// Tab on the phone step goes between its two fields.
    fn tab(&mut self, window: &mut Window, cx: &mut Context<Sheet>) {
        if !matches!(&self.face, Some(Face::Step(SignInStep::Phone { .. }))) {
            return;
        }
        let (country, number) = (self.country.focus_handle(cx), self.number.focus_handle(cx));
        window.focus(if number.is_focused(window) { &country } else { &number }, cx);
    }

    // Drawing.

    /// The first row: the way back on the later steps, the network's icon, the title.
    fn header(&self, title: &'static str, back: bool, cx: &Context<Sheet>) -> Div {
        let back = back.then(|| {
            div()
                .id("signin-back")
                .flex_none()
                .size(px(28.))
                // The arrow, not its hit area, lines up with the text below.
                .ml(px(-4.))
                .mr(px(-4.))
                .rounded(px(theme::R_CONTROL))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|style| style.bg(theme::hover()))
                .active(|style| style.bg(theme::pressed()))
                .child(glyph("icons/arrow-left.svg", 20., theme::text_soft()))
                .on_click(cx.listener(|this, _, _, cx| this.answer(SignInInput::Back, cx)))
        });
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .children(back)
            .child(network_icon(self.network, 28.))
            .child(div().flex_1().min_w_0().type_title().child(title))
    }

    fn cancel(&self, label: &'static str, cx: &Context<Sheet>) -> Stateful<Div> {
        button("signin-cancel", ButtonKind::Secondary, label).on_click(cx.listener(|this, _, _, cx| this.close(true, cx)))
    }

    /// A dialog's main action, off while an answer is awaited.
    fn primary(&self, label: &'static str, act: fn(&mut Sheet, &mut Context<Sheet>), cx: &Context<Sheet>) -> gpui::AnyElement {
        if self.busy {
            return button_disabled(label).h(px(32.)).into_any_element();
        }
        button("signin-primary", ButtonKind::Primary, label)
            .h(px(32.))
            .on_click(cx.listener(move |this, _, _, cx| act(this, cx)))
            .into_any_element()
    }

    /// A link that answers the step. It does nothing while an answer is awaited.
    fn answer_link(&self, id: &'static str, words: &'static str, input: fn() -> SignInInput, cx: &Context<Sheet>) -> Stateful<Div> {
        link(id, words).on_click(cx.listener(move |this, _, _, cx| this.answer(input(), cx)))
    }

    fn qr_step(&self, problem: Option<&SignInProblem>, cx: &Context<Sheet>) -> Div {
        let tile = match (problem, &self.code) {
            (Some(problem), _) => quiet_tile()
                .gap(px(12.))
                .child(div().text_center().child(problem_words(self.network, problem)))
                .child(button("signin-again", ButtonKind::Secondary, "Try again").on_click(cx.listener(|this, _, _, cx| this.restart(cx)))),
            (None, Some((_, code))) => code_tile(code.clone()),
            (None, None) => quiet_tile().child(div().text_center().child(format!("Getting a code from {}", self.network.label()))),
        };
        let phone_only = matches!(self.network, Network::Signal | Network::Messages | Network::Tiny);
        let steps = match self.network {
            Network::Signal => ["Open Signal on your phone.", "Go to Settings > Linked devices > Link new device.", "Point the phone at this code."],
            Network::Messages => ["Open Messages on your Android phone.", "Go to Settings > Device pairing > QR code scanner.", "Point the phone at this code."],
            Network::Tiny => ["The address of a server you already run.", "Then the email on that account.", "Then the password."],
            Network::Telegram | Network::Discord | Network::Slack | Network::Teams => ["Open Telegram on your phone.", "Go to Settings > Devices > Link Desktop Device.", "Point the phone at this code."],
        };
        // Signal and Messages link a computer only from the phone: there is no other way in to offer.
        let other_way = (!phone_only).then(|| self.answer_link("signin-use-phone", "Use a phone number instead", || SignInInput::UsePhone, cx));
        div()
            .flex()
            .flex_col()
            .child(self.header(self.title(), false, cx))
            .child(div().mt(px(20.)).mb(px(18.)).flex().justify_center().child(tile))
            .child(div().flex().flex_col().gap(px(8.)).children(steps.into_iter().enumerate().map(|(at, words)| numbered(at + 1, words))))
            .child(footer(22.).child(other_way.map_or_else(|| div().into_any_element(), IntoElement::into_any_element)).child(self.cancel("Cancel", cx)))
    }

    fn phone_step(&self, problem: Option<&SignInProblem>, window: &mut Window, cx: &Context<Sheet>) -> Div {
        let bad = problem.is_some();
        let input = |entity: &Entity<TextInput>| {
            let focused = entity.focus_handle(cx).is_focused(window);
            tall_field(focused, bad).child(entity.clone())
        };
        let country = input(&self.country).flex_none().w(px(96.));
        let number = input(&self.number).flex_1().min_w_0();
        div()
            .flex()
            .flex_col()
            .child(self.header("Your phone number", true, cx))
            .child(div().mt(px(16.)).flex().gap(px(8.)).child(country).child(number))
            .children(problem.map(|problem| error_line(problem_words(self.network, problem)).mt(px(6.))))
            .child(
                footer(22.)
                    .child(self.answer_link("signin-use-qr", "Scan a code instead", || SignInInput::UseQr, cx))
                    .child(buttons().child(self.cancel("Cancel", cx)).child(self.primary("Send code", Sheet::send_phone, cx))),
            )
    }

    #[allow(clippy::too_many_arguments)]
    fn code_step(
        &self,
        via: CodeVia,
        length: u8,
        phone: &str,
        next: Option<CodeVia>,
        resend_at: i64,
        problem: Option<&SignInProblem>,
        window: &mut Window,
        cx: &Context<Sheet>,
    ) -> Div {
        let sent = sent_words(via, length, phone);
        let length = usize::from(length).max(1);
        let handle = self.digits.focus_handle(cx);
        let focused = handle.is_focused(window);
        let typed: Vec<char> = self.digits.read(cx).text().chars().collect();
        let bad = problem.is_some();
        let surface = theme::over(theme::field(), theme::overlay());
        let boxes = (0..length).map(|at| {
            // The caret is the border of the box the next digit lands in.
            let caret = focused && !self.busy && at == typed.len().min(length - 1);
            let edge = if caret {
                theme::focus()
            } else if bad {
                theme::danger()
            } else {
                theme::border()
            };
            div()
                .flex_none()
                .w(px(BOX.0))
                .h(px(BOX.1))
                .rounded(px(theme::R_CONTROL))
                // Opaque: the one real field is under the first box.
                .bg(surface)
                .border_1()
                .border_color(edge)
                .when(caret, |cell| cell.shadow(theme::field_halo()))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(24.))
                .line_height(px(30.))
                .font_weight(theme::SEMIBOLD)
                .children(typed.get(at).map(|digit| digit.to_string()))
        });
        let expired = matches!(problem, Some(SignInProblem::CodeExpired));
        let again = || self.answer_link("signin-resend", "Send a new code", || SignInInput::Resend, cx);
        let error = problem.map(|problem| {
            div()
                .mt(px(12.))
                .flex()
                .justify_center()
                .items_center()
                .gap(px(6.))
                .child(error_line(problem_words(self.network, problem)).text_center())
                .children(expired.then(|| again().type_small()))
        });
        // Under the boxes, left: the other way to get a code, or when one may be asked for.
        let resend: gpui::AnyElement = match resend_words(next) {
            // The expired code's own link says the same.
            _ if expired => div().into_any_element(),
            Some(_) if resend_at > now() => {
                let words = format!("You can ask for a new code at {}.", clock(resend_at));
                div().type_ui().text_color(theme::text_soft()).child(words).into_any_element()
            }
            Some(words) => self.answer_link("signin-resend", words, || SignInInput::Resend, cx).into_any_element(),
            None => div().into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .child(self.header("Enter the code", true, cx))
            .child(div().mt(px(6.)).mb(px(22.)).text_color(theme::text_soft()).child(sent))
            .child(
                div().flex().justify_center().child(
                    div()
                        .relative()
                        .flex()
                        .gap(px(8.))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_, _, window, cx| {
                                window.focus(&handle, cx);
                                window.prevent_default();
                            }),
                        )
                        .child(hidden(&self.digits, 4., 18., 40.))
                        .children(boxes),
                ),
            )
            .children(error)
            .child(footer(26.).child(resend).child(self.cancel("Cancel", cx)))
    }

    fn password_step(&self, hint: &str, problem: Option<&SignInProblem>, window: &mut Window, cx: &Context<Sheet>) -> Div {
        let handle = self.password.focus_handle(cx);
        let focused = handle.is_focused(window);
        let count = self.password.read(cx).text().chars().count();
        let surface = theme::over(theme::field(), theme::overlay());
        // The field draws no letters: the real one is underneath, and this is what shows.
        let bullets = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .rounded(px(theme::R_CONTROL - 1.))
            .bg(surface)
            .px(px(12.))
            .flex()
            .items_center()
            .overflow_hidden()
            .map(|cover| {
                if count == 0 && !focused {
                    cover.child(div().text_color(theme::text_soft()).child("Password"))
                } else {
                    cover.child(div().flex_none().text_size(px(16.)).child("\u{2022}".repeat(count)))
                }
            })
            .children(focused.then(|| div().flex_none().ml(px(1.)).w(px(2.)).h(px(18.)).bg(theme::accent_text())));
        let secret = tall_field(focused, problem.is_some())
            .relative()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _, window, cx| {
                    window.focus(&handle, cx);
                    window.prevent_default();
                }),
            )
            .child(hidden(&self.password, 12., 10., 64.))
            .child(bullets);
        let tiny = self.network == Network::Tiny;
        let heading = if tiny { self.header("Password", true, cx) } else { self.header("Two-step verification password", true, cx) };
        let aside = if tiny {
            div().type_ui().text_color(theme::text_soft()).child("A password of 12 to 128 characters.")
        } else {
            // Words in a link's color, not a link: the reset happens in Telegram's own app.
            div().type_ui().text_color(theme::accent_text()).child("Forgot it? Reset it in the Telegram app.")
        };
        div()
            .flex()
            .flex_col()
            .child(heading)
            .child(div().mt(px(16.)).child(secret))
            .children(problem.map(|problem| error_line(problem_words(self.network, problem)).mt(px(6.))))
            // `text_faint` does not sit on `overlay` (`01-system.md`, rule 7): one level up.
            .children((!tiny && !hint.is_empty()).then(|| div().mt(px(6.)).type_small().text_color(theme::text_soft()).child(format!("Hint from Telegram: {hint}"))))
            .child(footer(22.).child(aside).child(buttons().child(self.cancel("Cancel", cx)).child(self.primary("Continue", Sheet::send_password, cx))))
    }

    fn server_step(&self, problem: Option<&SignInProblem>, window: &mut Window, cx: &Context<Sheet>) -> Div {
        let focused = self.url.focus_handle(cx).is_focused(window);
        let input = tall_field(focused, problem.is_some()).child(self.url.clone());
        div()
            .flex()
            .flex_col()
            .child(self.header("Server", false, cx))
            .child(div().mt(px(6.)).mb(px(16.)).text_color(theme::text_soft()).child("The host of a server you already run. https is added for you."))
            .child(input)
            .children(problem.map(|problem| error_line(problem_words(self.network, problem)).mt(px(6.))))
            .child(footer(22.).child(div()).child(buttons().child(self.cancel("Cancel", cx)).child(self.primary("Continue", Sheet::send_url, cx))))
            .child(div().mt(px(12.)).child(self.have_code(cx)))
    }

    /// "I have a code" reads the address already typed. `answer_link` cannot see the field.
    fn have_code(&self, cx: &Context<Sheet>) -> Stateful<Div> {
        link("signin-have-code", "I have a code").on_click(cx.listener(|this, _, _, cx| this.send_have_code(cx)))
    }

    fn email_step(&self, server: &str, problem: Option<&SignInProblem>, window: &mut Window, cx: &Context<Sheet>) -> Div {
        let focused = self.email.focus_handle(cx).is_focused(window);
        let input = tall_field(focused, problem.is_some()).child(self.email.clone());
        div()
            .flex()
            .flex_col()
            .child(self.header("Email", true, cx))
            .child(div().mt(px(6.)).mb(px(16.)).text_color(theme::text_soft()).child(server.to_owned()))
            .child(input)
            .children(problem.map(|problem| error_line(problem_words(self.network, problem)).mt(px(6.))))
            .child(footer(22.).child(div()).child(buttons().child(self.cancel("Cancel", cx)).child(self.primary("Continue", Sheet::send_email, cx))))
    }

    fn invite_step(&self, server: &str, problem: Option<&SignInProblem>, window: &mut Window, cx: &Context<Sheet>) -> Div {
        let focused = self.invite.focus_handle(cx).is_focused(window);
        let input = tall_field(focused, problem.is_some()).child(self.invite.clone());
        div()
            .flex()
            .flex_col()
            .child(self.header("Code", true, cx))
            .child(div().mt(px(6.)).mb(px(16.)).text_color(theme::text_soft()).child(server.to_owned()))
            .child(input)
            .children(problem.map(|problem| error_line(problem_words(self.network, problem)).mt(px(6.))))
            .child(footer(22.).child(div()).child(buttons().child(self.cancel("Cancel", cx)).child(self.primary("Continue", Sheet::send_invite, cx))))
    }

    fn name_step(&self, problem: Option<&SignInProblem>, window: &mut Window, cx: &Context<Sheet>) -> Div {
        let focused = self.name.focus_handle(cx).is_focused(window);
        let input = tall_field(focused, problem.is_some()).child(self.name.clone());
        div()
            .flex()
            .flex_col()
            .child(self.header("Name", true, cx))
            .child(div().mt(px(6.)).mb(px(16.)).text_color(theme::text_soft()).child("The name people on that server will see."))
            .child(input)
            .children(problem.map(|problem| error_line(problem_words(self.network, problem)).mt(px(6.))))
            .child(footer(22.).child(div()).child(buttons().child(self.cancel("Cancel", cx)).child(self.primary("Continue", Sheet::send_name, cx))))
    }

    fn title(&self) -> &'static str {
        match self.network {
            Network::Telegram => "Sign in to Telegram",
            Network::Signal => "Sign in to Signal",
            Network::Messages => "Sign in to Messages",
            Network::Tiny => "Sign in to Tiny server",
            Network::Discord | Network::Slack | Network::Teams => "Sign in to Telegram",
        }
    }

    /// The sign-in cannot go on: chatd's reason, and the way to start over.
    fn failed_step(&self, reason: &str, cx: &Context<Sheet>) -> Div {
        div()
            .flex()
            .flex_col()
            .child(self.header(self.title(), false, cx))
            .child(div().mt(px(8.)).text_color(theme::text_soft()).child(reason.to_owned()))
            .child(footer(20.).child(div()).child(buttons().child(self.cancel("Close", cx)).child(self.primary("Try again", Sheet::restart, cx))))
    }

    fn unsupported(&self, words: &str, cx: &Context<Sheet>) -> Div {
        div()
            .flex()
            .flex_col()
            .child(self.header(self.title(), false, cx))
            .child(div().mt(px(16.)).text_center().text_color(theme::text_soft()).child(words.to_owned()))
            .child(
                footer(20.).child(div()).child(
                    button("signin-close", ButtonKind::Secondary, "Close").on_click(cx.listener(|this, _, _, cx| this.close(false, cx))),
                ),
            )
    }
}

/// A line of words that acts: accent text, underlined under the pointer.
fn link(id: &'static str, words: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .type_ui()
        .text_color(theme::accent_text())
        .cursor_pointer()
        .hover(|style| style.underline())
        .child(words.into())
}

/// A problem, in the sheet's words: 12 in `danger`.
fn error_line(words: String) -> Div {
    div().type_small().text_color(theme::danger()).child(words)
}

/// The last row: something quiet on the left, the buttons on the right.
fn footer(above: f32) -> Div {
    div().mt(px(above)).flex().items_center().justify_between().gap(px(12.))
}

/// Buttons sit 8 apart, the main action last.
fn buttons() -> Div {
    div().flex_none().flex().items_center().gap(px(8.))
}

/// One instruction: its number on an 18 pt disc, then the words.
fn numbered(number: usize, words: &'static str) -> Div {
    div()
        .flex()
        .gap(px(10.))
        .text_color(theme::text_soft())
        .child(
            div()
                .flex_none()
                .size(px(18.))
                .rounded(px(9.))
                .bg(theme::selected())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.))
                .line_height(px(14.))
                .font_weight(theme::SEMIBOLD)
                .text_color(theme::text())
                .child(number.to_string()),
        )
        .child(words)
}

/// The sheet's text field: the system's field at 40 high. Opaque on the panel, so the halo of
/// a focused one does not show through it.
fn tall_field(focused: bool, bad: bool) -> Div {
    field(focused)
        .h(px(FIELD))
        .px(px(12.))
        .type_body()
        .bg(theme::over(theme::field(), theme::overlay()))
        .when(bad && !focused, |field| field.border_color(theme::danger()))
}

/// A real field placed under something opaque that draws its text another way: it takes the
/// keys, the paste and the input method, and shows nothing. Its own letters are transparent.
fn hidden(input: &Entity<TextInput>, left: f32, top: f32, width: f32) -> Div {
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .flex()
        .type_body()
        .text_color(gpui::transparent_black())
        .child(input.clone())
}

/// The tile before there is a code, or when there cannot be one: `raised`, with words.
fn quiet_tile() -> Div {
    div()
        .flex_none()
        .size(px(TILE))
        .rounded(px(TILE_RADIUS))
        .bg(theme::raised())
        .p(px(24.))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .text_color(theme::text_soft())
}

/// The code on its white tile: one canvas of filled squares, a whole number of points each,
/// centered in the room the padding leaves. The padding is the quiet zone a reader needs.
fn code_tile(code: Rc<qr::Qr>) -> Div {
    let squares = canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let modules = code.size();
            let (module, margin) = module_size(modules, f32::from(bounds.size.width));
            // On whole device pixels, wherever the sheet landed.
            let scale = window.scale_factor();
            let snap = |at: gpui::Pixels| px((f32::from(at) * scale).round() / scale);
            let origin = point(snap(bounds.origin.x) + px(margin), snap(bounds.origin.y) + px(margin));
            for y in 0..modules {
                // A run of dark modules in a row is one rectangle.
                let mut x = 0;
                while x < modules {
                    let from = x;
                    while x < modules && code.dark(x, y) {
                        x += 1;
                    }
                    if x > from {
                        let at = point(origin.x + px(module * from as f32), origin.y + px(module * y as f32));
                        window.paint_quad(fill(Bounds::new(at, size(px(module * (x - from) as f32), px(module))), ink()));
                    }
                    x += 1;
                }
            }
        },
    )
    .size(px(CODE_ROOM));
    div().flex_none().size(px(TILE)).rounded(px(TILE_RADIUS)).bg(rgb(QR_PAPER)).p(px(TILE_PAD)).child(squares)
}

fn ink() -> Rgba {
    rgb(QR_INK)
}

/// The panel: a modal of the system (`overlay`, its edge, radius 12, padding 20, 440 wide).
/// `components::modal` puts a bare title first; this sheet's first row has an icon and a way
/// back, so it builds the same panel around its own.
fn panel() -> Div {
    div()
        .w(px(440.))
        .p(px(20.))
        .rounded(px(theme::R_MODAL))
        .bg(theme::overlay())
        .border_1()
        .border_color(theme::overlay_edge())
        .shadow(theme::shadow_modal())
        .font_family(theme::FONT)
        .type_ui()
        .text_color(theme::text())
        .occlude()
}

/// The shell's keys that must not reach it through the sheet: Return would start another
/// sign-in on the first-run list behind it.
macro_rules! swallow {
    ($root:expr, $($action:ident),*) => {
        $root$(.on_action(|_: &super::$action, _: &mut Window, _: &mut App| {}))*
    };
}

impl Render for Sheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Sheet>) -> impl IntoElement {
        let Some(face) = self.face.clone() else {
            return div();
        };
        if std::mem::take(&mut self.refocus) {
            if self.before.is_none() && !self.focus.contains_focused(window, cx) {
                self.before = window.focused(cx);
            }
            let target = self.target(cx);
            window.defer(cx, move |window, cx| window.focus(&target, cx));
        }
        let body = match &face {
            Face::Unsupported(words) => self.unsupported(words, cx),
            Face::Step(step) => {
                // A problem found here shows where chatd's would.
                let problem = self.local.clone().or_else(|| step_problem(step).cloned());
                let problem = problem.as_ref();
                match step {
                    SignInStep::Qr { .. } => self.qr_step(problem, cx),
                    SignInStep::Phone { .. } => self.phone_step(problem, window, cx),
                    SignInStep::Code { via, length, phone, next, resend_at, .. } => {
                        self.code_step(*via, *length, phone, *next, *resend_at, problem, window, cx)
                    }
                    SignInStep::Password { hint, .. } => self.password_step(hint, problem, window, cx),
                    SignInStep::Server { .. } => self.server_step(problem, window, cx),
                    SignInStep::Email { server, .. } => self.email_step(server, problem, window, cx),
                    SignInStep::Invite { server, .. } => self.invite_step(server, problem, window, cx),
                    SignInStep::Name { .. } => self.name_step(problem, window, cx),
                    SignInStep::Failed { reason } => self.failed_step(reason, cx),
                    SignInStep::Done { .. } => div(),
                }
            }
        };
        let root = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .track_focus(&self.focus)
            .key_context("SignIn")
            .on_action(cx.listener(|this, _: &super::CloseTop, _, cx| this.close(true, cx)))
            .on_action(cx.listener(|this, _: &super::FocusNext, window, cx| this.tab(window, cx)))
            .on_action(cx.listener(|this, _: &super::FocusPrevious, window, cx| this.tab(window, cx)));
        let root = swallow!(
            root, ListOpen, ListUp, ListDown, ListFold, ListUnfold, MarkRead, Switcher, Back, Forward, UpLevel, ToggleSidebar,
            CloseRight, NextRow, PreviousRow, NextUnread, PreviousUnread, Place1, Place2, Place3, Place4, Place5, Place6, Place7,
            Place8, Place9
        );
        root.child(scrim()).child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(fade_in("signin-sheet", 160, panel().child(body))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phone_number_goes_out_as_plus_and_digits_or_not_at_all() {
        assert_eq!(phone_number("+1", "555 010 0142").as_deref(), Some("+15550100142"));
        assert_eq!(phone_number("44", "(020) 7946-0958").as_deref(), Some("+4402079460958"));
        // The country field may be empty when the number carries its own code.
        assert_eq!(phone_number("", "+49 30 901820").as_deref(), Some("+4930901820"));
        // Fewer than seven digits is not a number anywhere.
        assert_eq!(phone_number("+1", "55501"), None);
        assert_eq!(phone_number("+1", ""), None);
        assert_eq!(phone_number("+1", "call me"), None);
        assert_eq!(phone_shown("+1", " 555 010 0142 "), "+1 555 010 0142");
        assert_eq!(phone_shown("", "+49 30 901820"), "+49 30 901820");
    }

    #[test]
    fn the_boxes_keep_digits_only_and_no_more_than_they_hold() {
        assert_eq!(code_digits("41907", 5), "41907");
        assert_eq!(code_digits("4 1-9", 5), "419");
        // A pasted message fills all of them.
        assert_eq!(code_digits("Login code: 41907. Do not give it to anyone.", 5), "41907");
        assert_eq!(code_digits("123456789", 6), "123456");
        assert_eq!(code_digits("abc", 5), "");
    }

    #[test]
    fn the_sentence_says_how_the_code_was_sent_and_for_which_number() {
        assert_eq!(
            sent_words(CodeVia::App, 5, "+1 555 010 0142"),
            "Telegram sent a 5-digit code to the Telegram app on your other devices, for +1 555 010 0142."
        );
        assert_eq!(sent_words(CodeVia::Sms, 5, "+1 555 010 0142"), "Telegram sent a 5-digit code by SMS to +1 555 010 0142.");
        assert_eq!(sent_words(CodeVia::Sms, 6, ""), "Telegram sent a 6-digit code by SMS.");
        assert_eq!(sent_words(CodeVia::Call, 5, "+15550100142"), "Telegram is calling +15550100142 to read out a 5-digit code.");
        for via in [CodeVia::App, CodeVia::Sms, CodeVia::Call, CodeVia::Email, CodeVia::Other] {
            assert!(sent_words(via, 5, "+1 555").ends_with('.') && sent_words(via, 5, " ").ends_with('.'));
            assert!(!sent_words(via, 5, "").contains("  "));
        }
    }

    #[test]
    fn every_problem_has_the_words_of_the_design() {
        let words = |problem| problem_words(Network::Telegram, &problem);
        assert_eq!(words(SignInProblem::Unreachable), "Couldn't reach Telegram. Check your connection.");
        assert_eq!(words(SignInProblem::PhoneInvalid), "That doesn't look like a phone number.");
        assert_eq!(words(SignInProblem::PhoneUnknown), "No Telegram account uses this number. Create one in the Telegram app first.");
        assert_eq!(words(SignInProblem::PhoneBanned), "Telegram has blocked this number.");
        assert_eq!(words(SignInProblem::CodeInvalid), "That code isn't right. Check it and type it again.");
        assert_eq!(words(SignInProblem::CodeExpired), "That code has expired.");
        assert_eq!(words(SignInProblem::PasswordInvalid), "That password isn't right.");
        assert_eq!(words(SignInProblem::Other { reason: "Telegram is having trouble.".into() }), "Telegram is having trouble.");
        let until = 1_760_000_000;
        assert_eq!(words(SignInProblem::TooMany { until }), format!("Too many tries. Telegram allows another at {}.", clock(until)));
        // Signal is named for itself.
        assert_eq!(problem_words(Network::Signal, &SignInProblem::Unreachable), "Couldn't reach Signal. Check your connection.");
        assert_eq!(problem_words(Network::Tiny, &SignInProblem::UrlInvalid), "Type a host, like tiny.example.com.");
        assert_eq!(problem_words(Network::Tiny, &SignInProblem::EmailInvalid), "Type an email address, like name@example.com.");
        assert_eq!(problem_words(Network::Tiny, &SignInProblem::PasswordInvalid), "That email or password isn't right.");
        assert_eq!(problem_words(Network::Tiny, &SignInProblem::Unreachable), "Couldn't reach that server. Check the address and your connection.");
        assert_eq!(problem_words(Network::Tiny, &SignInProblem::TooMany { until }), "Too many tries. Wait a minute, then try again.");
        assert_eq!(asked_network("tiny"), Some(Network::Tiny));
        assert_eq!(asked_network("Tiny server"), Some(Network::Tiny));
    }

    #[test]
    fn the_resend_link_names_sms_only_when_that_is_the_next_way() {
        assert_eq!(resend_words(Some(CodeVia::Sms)), Some("Send the code by SMS"));
        assert_eq!(resend_words(Some(CodeVia::Call)), Some("Send a new code"));
        assert_eq!(resend_words(Some(CodeVia::App)), Some("Send a new code"));
        assert_eq!(resend_words(None), None);
    }

    #[test]
    fn the_code_is_drawn_at_a_whole_module_size_and_centered() {
        // 29 to 37 modules in 200 points: 6 or 5 points each, 174 to 198 across.
        assert_eq!(module_size(29, CODE_ROOM), (6., 13.));
        assert_eq!(module_size(33, CODE_ROOM), (6., 1.));
        assert_eq!(module_size(37, CODE_ROOM), (5., 7.));
        // The largest code made here still gets whole points.
        assert_eq!(module_size(57, CODE_ROOM), (3., 14.));
        for modules in (21..=57).step_by(4) {
            let (module, margin) = module_size(modules, CODE_ROOM);
            assert!(module >= 3. && module.fract() == 0. && margin.fract() == 0.);
            assert!(module * modules as f32 + 2. * margin <= CODE_ROOM);
        }
    }

    #[test]
    fn a_step_knows_its_problem() {
        let problem = Some(SignInProblem::CodeInvalid);
        let code = SignInStep::Code { via: CodeVia::App, length: 5, phone: String::new(), next: None, resend_at: 0, problem: problem.clone() };
        assert_eq!(step_problem(&code), problem.as_ref());
        assert_eq!(step_problem(&waiting()), None);
        assert_eq!(step_problem(&SignInStep::Failed { reason: "no".into() }), None);
    }
}
