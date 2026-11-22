// SPDX-License-Identifier: GPL-3.0-or-later

//! The guided account setup: address, sign in, connect, finish. The frame
//! stays the same in every step: progress on the left (a bar in narrow
//! windows), the question in the middle, "Back" and the main button below.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use e_mail_core::check::{self, Advice, CheckError, Stage};
use e_mail_core::discovery::net::RealNetwork;
use e_mail_core::discovery::providers::{self, Preparation, Provider, ProviderId};
use e_mail_core::discovery::{self, Discovery};
use e_mail_core::model::{AccountId, NewAccount, Security, ServerConfig, ServerSettings};
use e_mail_core::tls::Trust;
use futures::FutureExt;
use futures::future::{LocalBoxFuture, Shared};
use gettextrs::gettext;
use gtk::glib;

use crate::widgets::{self, fmt};
use crate::{runtime, secrets};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Email,
    Hint,
    Unsupported,
    NotFound,
    Password,
    Manual,
    /// Servers found through unsigned DNS that do not belong to the mail
    /// domain: shown before the password is sent anywhere.
    Confirm,
    Connect,
    Personal,
    Done,
}

impl Step {
    /// Position in the four-part progress.
    fn index(self) -> usize {
        match self {
            Step::Email | Step::Hint | Step::Unsupported | Step::NotFound => 0,
            Step::Password | Step::Manual => 1,
            Step::Confirm | Step::Connect => 2,
            Step::Personal => 3,
            Step::Done => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Next,
    Back,
    ChangeAddress,
    Another,
    Open,
    Manual,
}

type Collector = Box<dyn Fn(&mut State)>;
type DiscoveryFuture = Shared<LocalBoxFuture<'static, Discovery>>;

struct State {
    step: Step,
    email: String,
    password: String,
    error: Option<String>,
    error_offers_manual: bool,
    /// Where a failed connection attempt returns to.
    came_from: Step,
    discovery: Option<DiscoveryFuture>,
    settings: Option<ServerSettings>,
    detected: Option<ProviderId>,
    stage: usize,
    /// The user saw and accepted servers that need a look.
    confirmed: bool,
    name: String,
    label: String,
    added: Option<AccountId>,
}

impl State {
    fn new() -> State {
        State {
            step: Step::Email,
            email: String::new(),
            password: String::new(),
            error: None,
            error_offers_manual: false,
            came_from: Step::Password,
            discovery: None,
            settings: None,
            detected: None,
            stage: 0,
            confirmed: false,
            name: String::new(),
            label: String::new(),
            added: None,
        }
    }

    /// The provider as far as we know it: built in, or found through the
    /// mail servers of an own domain.
    fn provider(&self) -> Option<&'static Provider> {
        providers::provider_for_address(&self.email)
            .or_else(|| self.detected.map(providers::provider))
    }

    fn known_provider(&self) -> Option<&'static Provider> {
        self.provider()
            .filter(|provider| provider.preparation != Preparation::Unsupported)
    }
}

pub struct Hooks {
    /// Whether an account with this address exists already.
    pub is_known: Box<dyn Fn(&str) -> bool>,
    /// Saves the checked account. The password is already in the keyring.
    pub add: Box<dyn Fn(NewAccount, String) -> Result<AccountId, String>>,
    /// "Open Inbox" in the last step.
    pub open: Box<dyn Fn(AccountId)>,
}

struct Rail {
    dots: Vec<(gtk::Stack, gtk::Label)>,
    rows: Vec<gtk::Box>,
    subtitles: Vec<gtk::Label>,
    segments: Vec<gtk::Box>,
}

pub struct SetupFlow {
    pub widget: adw::BreakpointBin,
    content: gtk::Box,
    scroller: gtk::ScrolledWindow,
    rail: Rail,
    back: gtk::Button,
    fine: gtk::Label,
    secondary: gtk::Button,
    primary: gtk::Button,
    actions: RefCell<(Option<Action>, Option<Action>)>,
    state: RefCell<State>,
    /// Increased whenever the user leaves a step, so late answers from a
    /// connection attempt are ignored.
    generation: Cell<u64>,
    /// Reads the current input fields into the state before an action.
    collect: RefCell<Option<Collector>>,
    hooks: Hooks,
}

impl SetupFlow {
    pub fn new(hooks: Hooks) -> Rc<SetupFlow> {
        let header = adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new(&gettext("Add Account"), ""))
            .build();

        let (rail_widget, rail) = build_rail();

        let progress = gtk::Box::builder()
            .spacing(4)
            .margin_top(14)
            .margin_start(20)
            .margin_end(20)
            .visible(false)
            .homogeneous(true)
            .build();
        for segment in &rail.segments {
            progress.append(segment);
        }

        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        let clamp = adw::Clamp::builder()
            .maximum_size(560)
            .tightening_threshold(400)
            .child(&content)
            .build();
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["setup-inner"])
            .build();
        inner.append(&clamp);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&inner)
            .build();

        let back = widgets::icon_label_button("go-previous-symbolic", &gettext("Back"));
        back.add_css_class("flat");
        let fine = gtk::Label::builder()
            .label(gettext("Please wait a moment …"))
            .css_classes(["dim-label", "caption"])
            .visible(false)
            .build();
        let secondary = gtk::Button::builder().css_classes(["flat"]).build();
        let primary = gtk::Button::builder()
            .css_classes(["suggested-action"])
            .build();
        let spacer = gtk::Box::builder().hexpand(true).build();
        let footer = gtk::Box::builder()
            .spacing(8)
            .css_classes(["setup-footer"])
            .build();
        footer.append(&back);
        footer.append(&spacer);
        footer.append(&fine);
        footer.append(&secondary);
        footer.append(&primary);

        let pane = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        pane.append(&progress);
        pane.append(&scroller);
        pane.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        pane.append(&footer);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&rail_widget);
        body.append(&pane);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&body));

        let widget = adw::BreakpointBin::builder()
            .width_request(360)
            .height_request(294)
            .child(&view)
            .build();
        let narrow = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 640sp").expect("valid condition"),
        );
        narrow.add_setter(&rail_widget, "visible", Some(&false.to_value()));
        narrow.add_setter(&progress, "visible", Some(&true.to_value()));
        widget.add_breakpoint(narrow);

        let this = Rc::new(SetupFlow {
            widget,
            content,
            scroller,
            rail,
            back,
            fine,
            secondary,
            primary,
            actions: RefCell::new((None, None)),
            state: RefCell::new(State::new()),
            generation: Cell::new(0),
            collect: RefCell::default(),
            hooks,
        });
        let weak = Rc::downgrade(&this);
        this.primary.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let action = this.actions.borrow().0;
                if let Some(action) = action {
                    this.act(action);
                }
            }
        });
        let weak = Rc::downgrade(&this);
        this.secondary.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let action = this.actions.borrow().1;
                if let Some(action) = action {
                    this.act(action);
                }
            }
        });
        let weak = Rc::downgrade(&this);
        this.back.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.act(Action::Back);
            }
        });
        this.render();
        this
    }

    /// Starts over with an empty address, for example for another account.
    pub fn reset(self: &Rc<Self>) {
        self.leave();
        *self.state.borrow_mut() = State::new();
        self.render();
    }

    fn leave(&self) {
        self.generation.set(self.generation.get() + 1);
    }

    fn go(self: &Rc<Self>, step: Step) {
        self.leave();
        self.state.borrow_mut().step = step;
        self.render();
    }

    fn fail(self: &Rc<Self>, message: String) {
        {
            let mut state = self.state.borrow_mut();
            state.error = Some(message);
            state.error_offers_manual = false;
        }
        self.render();
    }

    fn act(self: &Rc<Self>, action: Action) {
        if let Some(collect) = self.collect.borrow().as_ref() {
            collect(&mut self.state.borrow_mut());
        }
        let step = self.state.borrow().step;
        self.state.borrow_mut().error = None;
        match action {
            Action::Back => {
                let provider = self.state.borrow().provider();
                let target = match step {
                    Step::Password
                        if provider.is_some_and(|p| {
                            matches!(
                                p.preparation,
                                Preparation::AppPassword | Preparation::EnableImap
                            )
                        }) =>
                    {
                        Step::Hint
                    }
                    _ => Step::Email,
                };
                self.go(target);
            }
            Action::ChangeAddress => self.go(Step::Email),
            Action::Another => self.reset(),
            Action::Manual => self.open_manual(),
            Action::Open => {
                if let Some(id) = self.state.borrow().added {
                    (self.hooks.open)(id);
                }
            }
            Action::Next => match step {
                Step::Email => self.submit_email(),
                Step::Hint => self.go(Step::Password),
                Step::Unsupported => self.go(Step::Email),
                Step::Password => self.submit_password(),
                Step::Manual => self.submit_manual(),
                Step::Personal => self.submit_personal(),
                Step::Confirm => {
                    let settings = self.state.borrow().settings.clone();
                    self.state.borrow_mut().confirmed = true;
                    self.connect(settings);
                }
                Step::NotFound | Step::Connect | Step::Done => {}
            },
        }
    }

    fn submit_email(self: &Rc<Self>) {
        let email = self.state.borrow().email.trim().to_owned();
        if email.is_empty() {
            return self.fail(gettext("Please enter your email address."));
        }
        if !providers::is_valid_address(&email) {
            return self.fail(gettext(
                "This address looks incomplete. It needs an @ and an ending, for example name@posteo.de.",
            ));
        }
        if (self.hooks.is_known)(&email) {
            return self.fail(gettext("This account has already been added."));
        }
        let provider = providers::provider_for_address(&email);
        {
            let mut state = self.state.borrow_mut();
            state.email = email.clone();
            state.detected = None;
            state.settings = None;
            state.confirmed = false;
            state.discovery = Some(start_discovery(email));
        }
        self.go(match provider.map(|p| p.preparation) {
            Some(Preparation::Unsupported) => Step::Unsupported,
            Some(Preparation::AppPassword | Preparation::EnableImap) => Step::Hint,
            _ => Step::Password,
        });
    }

    fn submit_password(self: &Rc<Self>) {
        if self.state.borrow().password.is_empty() {
            return self.fail(gettext("Please enter your password."));
        }
        self.state.borrow_mut().came_from = Step::Password;
        self.connect(None);
    }

    fn submit_manual(self: &Rc<Self>) {
        let (email, settings, password) = {
            let state = self.state.borrow();
            (
                state.email.trim().to_owned(),
                state.settings.clone(),
                state.password.clone(),
            )
        };
        if !providers::is_valid_address(&email) {
            return self.fail(gettext(
                "This address looks incomplete. It needs an @ and an ending, for example name@posteo.de.",
            ));
        }
        if (self.hooks.is_known)(&email) {
            return self.fail(gettext("This account has already been added."));
        }
        let Some(settings) = settings.filter(|s| {
            !s.imap.host.is_empty() && !s.smtp.host.is_empty() && !s.username.is_empty()
        }) else {
            return self.fail(gettext(
                "Please fill in the server names and the user name.",
            ));
        };
        if password.is_empty() {
            return self.fail(gettext("Please enter your password."));
        }
        self.state.borrow_mut().came_from = Step::Manual;
        self.connect(Some(settings));
    }

    /// Runs the connection check. Without `manual` settings it first waits
    /// for the automatic search.
    fn connect(self: &Rc<Self>, manual: Option<ServerSettings>) {
        self.state.borrow_mut().stage = 0;
        self.go(Step::Connect);
        let generation = self.generation.get();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let current = || this.generation.get() == generation;
            let settings = match manual {
                Some(settings) => settings,
                None => {
                    let pending = this.state.borrow().discovery.clone();
                    let discovery = match pending {
                        Some(pending) => pending.await,
                        None => Discovery::NotFound,
                    };
                    if !current() {
                        return;
                    }
                    match discovery {
                        Discovery::Found {
                            settings,
                            provider,
                            source,
                        } => {
                            this.state.borrow_mut().detected = provider;
                            let email = this.state.borrow().email.clone();
                            if !this.state.borrow().confirmed
                                && discovery::needs_confirmation(
                                    &email, &settings, source, provider,
                                )
                            {
                                this.state.borrow_mut().settings = Some(settings);
                                return this.go(Step::Confirm);
                            }
                            settings
                        }
                        Discovery::Unsupported => return this.go(Step::Unsupported),
                        Discovery::NotFound => return this.go(Step::NotFound),
                    }
                }
            };
            this.state.borrow_mut().settings = Some(settings.clone());

            let (progress, stages) = async_channel::unbounded();
            let password = this.state.borrow().password.clone();
            let check_settings = settings.clone();
            let check = runtime::run(async move {
                check::check_account(&check_settings, &password, &Trust::system(), |stage| {
                    let _ = progress.try_send(stage);
                })
                .await
            });
            let watcher = {
                let this = this.clone();
                async move {
                    while let Ok(stage) = stages.recv().await {
                        if this.generation.get() != generation {
                            return;
                        }
                        this.state.borrow_mut().stage = match stage {
                            Stage::SignIn => 1,
                            Stage::OpenMailbox => 2,
                        };
                        this.render();
                    }
                }
            };
            let (result, ()) = futures::join!(check, watcher);
            if !current() {
                return;
            }
            match result {
                Ok(()) => {
                    {
                        let mut state = this.state.borrow_mut();
                        if state.name.is_empty() {
                            state.name = guess_name(&state.email);
                        }
                        if state.label.is_empty() {
                            state.label = guess_label(&state.email, state.known_provider());
                        }
                    }
                    this.go(Step::Personal);
                }
                Err(error) => this.connection_failed(&error, &settings),
            }
        });
    }

    fn connection_failed(self: &Rc<Self>, error: &CheckError, settings: &ServerSettings) {
        tracing::info!("account check failed: {error}");
        let back = {
            let mut state = self.state.borrow_mut();
            let provider = state.known_provider();
            state.error = Some(advice_message(error, provider, settings));
            state.error_offers_manual = !error.is_login();
            if error.is_login() {
                state.password.clear();
            }
            state.came_from
        };
        self.leave();
        self.state.borrow_mut().step = back;
        self.render();
    }

    fn submit_personal(self: &Rc<Self>) {
        let (account, password) = {
            let mut state = self.state.borrow_mut();
            state.name = state.name.trim().to_owned();
            state.label = state.label.trim().to_owned();
            if state.label.is_empty() {
                state.label = gettext("New Account");
            }
            let Some(settings) = state.settings.clone() else {
                return;
            };
            (
                NewAccount {
                    email: state.email.clone(),
                    label: state.label.clone(),
                    display_name: state.name.clone(),
                    settings,
                },
                state.password.clone(),
            )
        };
        if (self.hooks.is_known)(&account.email) {
            return self.fail(gettext("This account has already been added."));
        }
        self.primary.set_sensitive(false);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let saved = runtime::run(secrets::save_password(
                account.email.clone(),
                password.clone(),
            ))
            .await;
            this.primary.set_sensitive(true);
            if let Err(error) = saved {
                tracing::warn!("keyring: {error}");
                return this.fail(gettext(
                    "The password could not be saved in the system keyring. Please make sure the keyring is unlocked and try again.",
                ));
            }
            match (this.hooks.add)(account, password) {
                Ok(id) => {
                    {
                        let mut state = this.state.borrow_mut();
                        state.added = Some(id);
                        state.password.clear();
                    }
                    this.go(Step::Done);
                }
                Err(message) => this.fail(message),
            }
        });
    }

    fn set_actions(&self, primary: Option<(String, Action)>, secondary: Option<(String, Action)>) {
        self.primary.set_visible(primary.is_some());
        self.secondary.set_visible(secondary.is_some());
        if let Some((label, _)) = &primary {
            self.primary.set_label(label);
        }
        if let Some((label, _)) = &secondary {
            self.secondary.set_label(label);
        }
        *self.actions.borrow_mut() = (
            primary.map(|(_, action)| action),
            secondary.map(|(_, action)| action),
        );
    }

    fn render(self: &Rc<Self>) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        *self.collect.borrow_mut() = None;
        self.primary.set_sensitive(true);
        let step = self.state.borrow().step;
        self.update_rail();
        self.back.set_visible(matches!(
            step,
            Step::Hint | Step::NotFound | Step::Password | Step::Manual
        ));
        self.fine.set_visible(step == Step::Connect);
        match step {
            Step::Email => self.render_email(),
            Step::Hint => self.render_hint(),
            Step::Unsupported => self.render_unsupported(),
            Step::NotFound => self.render_not_found(),
            Step::Password => self.render_password(),
            Step::Manual => self.render_manual(),
            Step::Confirm => self.render_confirm(),
            Step::Connect => self.render_connect(),
            Step::Personal => self.render_personal(),
            Step::Done => self.render_done(),
        }
        self.scroller.vadjustment().set_value(0.0);
    }

    fn update_rail(&self) {
        let state = self.state.borrow();
        let index = state.step.index();
        let provider = state.known_provider();
        let subtitles = [
            if index > 0 && !state.email.is_empty() {
                state.email.clone()
            } else {
                gettext("Which address?")
            },
            if index > 1 {
                gettext("Password checked")
            } else {
                provider.map_or_else(|| gettext("Password"), |p| p.name.to_owned())
            },
            if index > 2 {
                gettext("Mailbox connected")
            } else {
                gettext("Open mailbox")
            },
            if index > 3 {
                state.label.clone()
            } else {
                gettext("Name and account name")
            },
        ];
        for (i, subtitle) in subtitles.iter().enumerate() {
            self.rail.subtitles[i].set_label(subtitle);
            let (stack, _) = &self.rail.dots[i];
            stack.set_visible_child_name(if i < index { "done" } else { "number" });
            let row = &self.rail.rows[i];
            for class in ["past", "current", "pending"] {
                row.remove_css_class(class);
            }
            row.add_css_class(match i.cmp(&index) {
                std::cmp::Ordering::Less => "past",
                std::cmp::Ordering::Equal => "current",
                std::cmp::Ordering::Greater => "pending",
            });
            let segment = &self.rail.segments[i];
            if i <= index {
                segment.add_css_class("done");
            } else {
                segment.remove_css_class("done");
            }
        }
    }

    fn header(&self, eyebrow: &str, title: &str, lead: Option<&str>) {
        self.content.append(
            &gtk::Label::builder()
                .label(eyebrow.to_uppercase())
                .xalign(0.0)
                .css_classes(["setup-eyebrow", "dim-label"])
                .build(),
        );
        self.content.append(
            &gtk::Label::builder()
                .label(title)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["title-1"])
                .accessible_role(gtk::AccessibleRole::Heading)
                .build(),
        );
        if let Some(lead) = lead {
            self.content.append(
                &gtk::Label::builder()
                    .label(lead)
                    .use_markup(true)
                    .xalign(0.0)
                    .wrap(true)
                    .css_classes(["dim-label", "setup-lead"])
                    .build(),
            );
        }
    }

    fn step_eyebrow(&self, extra: Option<&str>) -> String {
        let index = self.state.borrow().step.index().min(3) + 1;
        let base = fmt(&gettext("Step {} of 4"), &[&index.to_string()]);
        match extra {
            Some(extra) => format!("{base} · {extra}"),
            None => base,
        }
    }

    fn append_error(self: &Rc<Self>) {
        let (error, offers_manual) = {
            let state = self.state.borrow();
            (state.error.clone(), state.error_offers_manual)
        };
        let Some(error) = error else {
            return;
        };
        let box_ = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .css_classes(["setup-error"])
            .accessible_role(gtk::AccessibleRole::Alert)
            .build();
        box_.append(
            &gtk::Label::builder()
                .label(&error)
                .xalign(0.0)
                .wrap(true)
                .build(),
        );
        if offers_manual && self.state.borrow().step == Step::Password {
            let manual = gtk::Button::builder()
                .label(gettext("Enter Servers Yourself"))
                .halign(gtk::Align::Start)
                .build();
            let weak = Rc::downgrade(self);
            manual.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.open_manual();
                }
            });
            box_.append(&manual);
        }
        self.content.append(&box_);
    }

    fn open_manual(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            state.error = None;
            if state.settings.is_none() {
                state.settings = Some(default_settings(&state.email));
            }
        }
        self.go(Step::Manual);
    }

    fn entry_activates_primary(self: &Rc<Self>, row: &impl IsA<adw::EntryRow>) {
        let weak = Rc::downgrade(self);
        row.as_ref().connect_entry_activated(move |_| {
            if let Some(this) = weak.upgrade() {
                this.act(Action::Next);
            }
        });
    }

    fn render_email(self: &Rc<Self>) {
        self.header(
            &self.step_eyebrow(None),
            &gettext("Which address would you like to add?"),
            Some(&gettext(
                "We recognise your provider and set up everything else for you.",
            )),
        );
        let entry = adw::EntryRow::builder()
            .title(gettext("Email address"))
            .text(self.state.borrow().email.as_str())
            .input_purpose(gtk::InputPurpose::Email)
            .build();
        self.content
            .append(&widgets::boxed_list(&[entry.upcast_ref()]));
        let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        self.content.append(&slot);
        self.append_error();

        let update = {
            let slot = slot.clone();
            move |email: &str| {
                while let Some(child) = slot.first_child() {
                    slot.remove(&child);
                }
                slot.append(&email_status(email));
            }
        };
        update(&self.state.borrow().email);
        let weak = Rc::downgrade(self);
        entry.connect_changed(move |entry| {
            update(&entry.text());
            if let Some(this) = weak.upgrade() {
                this.state.borrow_mut().email = entry.text().to_string();
            }
        });
        self.entry_activates_primary(&entry);
        let entry_ref = entry.clone();
        *self.collect.borrow_mut() = Some(Box::new(move |state: &mut State| {
            state.email = entry_ref.text().trim().to_owned();
        }));
        self.set_actions(Some((gettext("Continue"), Action::Next)), None);
        entry.grab_focus();
    }

    fn render_hint(self: &Rc<Self>) {
        let Some(provider) = self.state.borrow().known_provider() else {
            return;
        };
        let app_password = provider.preparation == Preparation::AppPassword;
        let name = provider.name;
        self.header(
            &self.step_eyebrow(Some(&gettext("Preparation"))),
            &if app_password {
                gettext("Create an app password")
            } else {
                fmt(&gettext("Allow access at {}"), &[name])
            },
            Some(&if app_password {
                fmt(
                    &gettext(
                        "For security reasons, {} does not accept your normal password in mail apps. An app password is created in two minutes.",
                    ),
                    &[name],
                )
            } else {
                fmt(
                    &gettext(
                        "{} only lets mail apps in after you allow it once. This takes about a minute.",
                    ),
                    &[name],
                )
            }),
        );
        let subtitle = if app_password {
            gettext("Requires an app password for mail apps")
        } else {
            gettext("Access for mail apps is switched off at first")
        };
        self.content
            .append(&widgets::card(name, name, &subtitle, None, None));

        let steps = gtk::Box::new(gtk::Orientation::Vertical, 12);
        for (i, text) in preparation_steps(provider.id).iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.append(
                &gtk::Label::builder()
                    .label((i + 1).to_string())
                    .valign(gtk::Align::Start)
                    .css_classes(["setup-number"])
                    .build(),
            );
            row.append(
                &gtk::Label::builder()
                    .label(text)
                    .xalign(0.0)
                    .wrap(true)
                    .hexpand(true)
                    .build(),
            );
            steps.append(&row);
        }
        self.content.append(&steps);

        if let (Some(link), Some(url)) = (provider.help_link, provider.help_url()) {
            let row = adw::ActionRow::builder()
                .title(fmt(&gettext("Open the instructions from {}"), &[name]))
                .subtitle(link)
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.connect_activated(move |row| {
                let window = row.root().and_downcast::<gtk::Window>();
                gtk::UriLauncher::new(&url).launch(
                    window.as_ref(),
                    gtk::gio::Cancellable::NONE,
                    |result| {
                        if let Err(error) = result {
                            tracing::warn!("could not open the help page: {error}");
                        }
                    },
                );
            });
            self.content
                .append(&widgets::boxed_list(&[row.upcast_ref()]));
        }
        self.set_actions(
            Some((
                if app_password {
                    gettext("I Have an App Password")
                } else {
                    gettext("Done, Continue")
                },
                Action::Next,
            )),
            None,
        );
    }

    fn render_unsupported(self: &Rc<Self>) {
        self.header(
            &gettext("Note"),
            &gettext("Microsoft accounts are not supported"),
            Some(&gettext(
                "Microsoft no longer lets mail apps like this one sign in with a password. Unfortunately, there is nothing we can do about that.",
            )),
        );
        let email = self.state.borrow().email.clone();
        self.content.append(&widgets::card(
            "M",
            &email,
            &gettext("Outlook, Hotmail and Live"),
            Some("dialog-warning-symbolic"),
            Some("warning"),
        ));
        self.content.append(&widgets::note(
            "mail-unread-symbolic",
            &gettext(
                "Your address keeps working in the browser on outlook.com and in the Outlook app.",
            ),
        ));
        self.set_actions(
            Some((gettext("Enter a Different Address"), Action::Next)),
            None,
        );
    }

    fn render_not_found(self: &Rc<Self>) {
        let domain = providers::split_address(&self.state.borrow().email)
            .map(|(_, domain)| domain)
            .unwrap_or_default();
        self.header(
            &self.step_eyebrow(None),
            &gettext("Settings not found"),
            Some(&fmt(
                &gettext(
                    "We could not find the server settings for <b>{}</b> automatically. How would you like to continue?",
                ),
                &[&glib::markup_escape_text(&domain)],
            )),
        );
        let tiles = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(2)
            .min_children_per_line(1)
            .homogeneous(true)
            .column_spacing(12)
            .row_spacing(12)
            .build();
        let check = widgets::tile(
            "mail-send-receive-symbolic",
            &gettext("Check Address"),
            &gettext("Maybe a typo has crept in."),
        );
        let weak = Rc::downgrade(self);
        check.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.act(Action::ChangeAddress);
            }
        });
        let manual = widgets::tile(
            "network-server-symbolic",
            &gettext("Enter Servers Yourself"),
            &gettext("The details are on your provider's help page."),
        );
        let weak = Rc::downgrade(self);
        manual.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.open_manual();
            }
        });
        tiles.append(&check);
        tiles.append(&manual);
        self.content.append(&tiles);
        self.set_actions(None, None);
    }

    fn render_password(self: &Rc<Self>) {
        let (email, provider, password) = {
            let state = self.state.borrow();
            (
                state.email.clone(),
                state.known_provider(),
                state.password.clone(),
            )
        };
        let app_password = provider.is_some_and(|p| p.preparation == Preparation::AppPassword);
        self.header(
            &self.step_eyebrow(None),
            &if app_password {
                gettext("Enter the app password")
            } else {
                gettext("Enter your password")
            },
            Some(&if let (true, Some(provider)) = (app_password, provider) {
                fmt(
                    &gettext("The app password you just created at {}. Not your normal password."),
                    &[provider.name],
                )
            } else {
                gettext("The same password you use to sign in at your provider.")
            }),
        );
        let change = gtk::Button::builder()
            .label(gettext("Change"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let weak = Rc::downgrade(self);
        change.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.act(Action::ChangeAddress);
            }
        });
        let letter = provider.map_or(email.as_str(), |p| p.name);
        let subtitle = provider.map_or_else(|| gettext("Own provider"), |p| p.name.to_owned());
        let card = widgets::card(letter, &email, &subtitle, None, None);
        widgets::card_row(&card).add_suffix(&change);
        self.content.append(&card);

        let entry = adw::PasswordEntryRow::builder()
            .title(if app_password {
                gettext("App password")
            } else {
                gettext("Password")
            })
            .text(password.as_str())
            .build();
        self.content
            .append(&widgets::boxed_list(&[entry.upcast_ref()]));
        self.append_error();
        self.content.append(&widgets::note(
            "channel-secure-symbolic",
            &gettext(
                "Stored safely in the keyring and only leaves this device to sign in to your provider.",
            ),
        ));
        self.entry_activates_primary(&entry);
        let entry_ref = entry.clone();
        *self.collect.borrow_mut() = Some(Box::new(move |state: &mut State| {
            state.password = entry_ref.text().to_string();
        }));
        self.set_actions(Some((gettext("Sign In"), Action::Next)), None);
        entry.grab_focus();
    }

    fn render_manual(self: &Rc<Self>) {
        self.header(
            &self.step_eyebrow(Some(&gettext("Manual"))),
            &gettext("Enter servers yourself"),
            Some(&gettext(
                "You can find the details on your provider's help page, usually under “IMAP settings”.",
            )),
        );
        let (email, password, settings) = {
            let state = self.state.borrow();
            (
                state.email.clone(),
                state.password.clone(),
                state
                    .settings
                    .clone()
                    .unwrap_or_else(|| default_settings(&state.email)),
            )
        };

        let email_row = adw::EntryRow::builder()
            .title(gettext("Email address"))
            .text(email.as_str())
            .input_purpose(gtk::InputPurpose::Email)
            .build();
        let username_row = adw::EntryRow::builder()
            .title(gettext("User name"))
            .text(settings.username.as_str())
            .build();
        let password_row = adw::PasswordEntryRow::builder()
            .title(gettext("Password"))
            .text(password.as_str())
            .build();
        // Above the fields, so it is visible without scrolling.
        self.append_error();
        let account = adw::PreferencesGroup::builder()
            .title(gettext("Account"))
            .build();
        account.add(&email_row);
        account.add(&username_row);
        account.add(&password_row);
        self.content.append(&account);

        let imap = ServerFields::new(&gettext("Incoming mail (IMAP)"), &settings.imap);
        let smtp = ServerFields::new(&gettext("Outgoing mail (SMTP)"), &settings.smtp);
        self.content.append(&imap.group);
        self.content.append(&smtp.group);

        for row in [&email_row, &username_row, password_row.upcast_ref()] {
            self.entry_activates_primary(row);
        }
        self.entry_activates_primary(&imap.host);
        self.entry_activates_primary(&smtp.host);
        {
            // The user name follows the address until it is edited by hand.
            let username_row = username_row.clone();
            email_row.connect_changed(move |email_row| {
                let previous = email_row.text();
                if username_row.text().is_empty()
                    || providers::is_valid_address(&username_row.text())
                {
                    username_row.set_text(previous.trim());
                }
            });
        }
        *self.collect.borrow_mut() = Some(Box::new(move |state: &mut State| {
            state.email = email_row.text().trim().to_owned();
            state.password = password_row.text().to_string();
            state.settings = Some(ServerSettings {
                imap: imap.config(),
                smtp: smtp.config(),
                username: username_row.text().trim().to_owned(),
            });
        }));
        self.set_actions(Some((gettext("Connect"), Action::Next)), None);
    }

    fn render_confirm(self: &Rc<Self>) {
        let (email, settings) = {
            let state = self.state.borrow();
            (state.email.clone(), state.settings.clone())
        };
        let Some(settings) = settings else {
            return;
        };
        let domain = providers::split_address(&email)
            .map(|(_, domain)| domain.to_owned())
            .unwrap_or_default();
        self.header(
            &self.step_eyebrow(None),
            &gettext("Check the mail server"),
            Some(&fmt(
                &gettext(
                    "These servers were found for {}, but they do not belong to that domain. Only continue if they are your provider's; otherwise enter the servers yourself.",
                ),
                &[&domain],
            )),
        );
        let rows: Vec<gtk::Widget> = [
            (gettext("Incoming mail (IMAP)"), &settings.imap),
            (gettext("Outgoing mail (SMTP)"), &settings.smtp),
        ]
        .into_iter()
        .map(|(title, server)| {
            adw::ActionRow::builder()
                .title(title)
                .subtitle(glib::markup_escape_text(&format!(
                    "{}:{}",
                    server.host, server.port
                )))
                .subtitle_selectable(true)
                .build()
                .upcast()
        })
        .collect();
        let refs: Vec<&gtk::Widget> = rows.iter().collect();
        self.content.append(&widgets::boxed_list(&refs));
        self.set_actions(
            Some((gettext("Connect"), Action::Next)),
            Some((gettext("Enter Servers Yourself"), Action::Manual)),
        );
    }

    fn render_connect(self: &Rc<Self>) {
        self.header(
            &self.step_eyebrow(None),
            &gettext("Connecting your mailbox"),
            Some(&gettext("This usually takes only a few seconds.")),
        );
        let (stage, manual, provider) = {
            let state = self.state.borrow();
            (
                state.stage,
                state.came_from == Step::Manual,
                state.known_provider(),
            )
        };
        let first = if manual {
            gettext("Reach the server")
        } else {
            match provider {
                Some(provider) => fmt(&gettext("Find settings for {}"), &[provider.name]),
                None => gettext("Find settings for your provider"),
            }
        };
        let checks = gtk::Box::new(gtk::Orientation::Vertical, 14);
        for (i, label) in [first, gettext("Sign in"), gettext("Open mailbox")]
            .iter()
            .enumerate()
        {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            let indicator: gtk::Widget = match i.cmp(&stage) {
                std::cmp::Ordering::Less => gtk::Image::from_icon_name("object-select-symbolic")
                    .tap(|image| image.add_css_class("success"))
                    .upcast(),
                std::cmp::Ordering::Equal => {
                    gtk::Spinner::builder().spinning(true).build().upcast()
                }
                std::cmp::Ordering::Greater => gtk::Box::builder()
                    .css_classes(["setup-pending"])
                    .valign(gtk::Align::Center)
                    .build()
                    .upcast(),
            };
            indicator.set_size_request(16, 16);
            row.append(&indicator);
            let text = gtk::Label::builder().label(label).xalign(0.0).build();
            if i > stage {
                text.add_css_class("dim-label");
            }
            row.append(&text);
            checks.append(&row);
        }
        self.content.append(&checks);
        self.set_actions(None, None);
    }

    fn render_personal(self: &Rc<Self>) {
        self.header(
            &self.step_eyebrow(None),
            &gettext("Connected! Two more things"),
            Some(&gettext(
                "We have already suggested something. You can change both later in the settings.",
            )),
        );
        let (name, label, email) = {
            let state = self.state.borrow();
            (state.name.clone(), state.label.clone(), state.email.clone())
        };
        let name_row = adw::EntryRow::builder()
            .title(gettext("Your name"))
            .text(name.as_str())
            .build();
        self.content.append(&widgets::field(
            &name_row,
            &gettext("This is how the recipients of your emails see you."),
        ));
        let label_row = adw::EntryRow::builder()
            .title(gettext("Name for this account"))
            .text(label.as_str())
            .build();
        self.content.append(&widgets::field(
            &label_row,
            &gettext("Only visible to you, for example “Personal” or “Club”."),
        ));

        let preview = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .css_classes(["setup-preview"])
            .build();
        preview.append(
            &gtk::Label::builder()
                .label(gettext("How it looks to recipients"))
                .xalign(0.0)
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        let avatar = adw::Avatar::new(36, Some(&name), true);
        let shown_name = gtk::Label::builder()
            .label(name.as_str())
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.append(&shown_name);
        text.append(
            &gtk::Label::builder()
                .label(email.as_str())
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.append(&avatar);
        row.append(&text);
        preview.append(&row);
        self.content.append(&preview);
        self.append_error();

        name_row.connect_changed(move |row| {
            let text = row.text();
            shown_name.set_label(&text);
            avatar.set_text(Some(&text));
        });
        self.entry_activates_primary(&name_row);
        self.entry_activates_primary(&label_row);
        *self.collect.borrow_mut() = Some(Box::new(move |state: &mut State| {
            state.name = name_row.text().to_string();
            state.label = label_row.text().to_string();
        }));
        self.set_actions(Some((gettext("Done"), Action::Next)), None);
    }

    fn render_done(self: &Rc<Self>) {
        let success = gtk::Image::builder()
            .icon_name("object-select-symbolic")
            .pixel_size(32)
            .halign(gtk::Align::Start)
            .css_classes(["setup-success"])
            .build();
        self.content.append(&success);
        self.header(
            &gettext("Finished"),
            &gettext("All set"),
            Some(&gettext(
                "The emails from the last 30 days are loading now. Older ones follow in the background, and you can start right away.",
            )),
        );
        let (label, email) = {
            let state = self.state.borrow();
            (state.label.clone(), state.email.clone())
        };
        self.content.append(&widgets::card(
            &label,
            &label,
            &email,
            Some("object-select-symbolic"),
            Some("success"),
        ));
        self.set_actions(
            Some((gettext("Open Inbox"), Action::Open)),
            Some((gettext("Another Account"), Action::Another)),
        );
        self.primary.grab_focus();
    }
}

/// The text for a failed check, fitting the provider.
pub fn advice_message(
    error: &CheckError,
    provider: Option<&Provider>,
    settings: &ServerSettings,
) -> String {
    let name = provider.map_or("", |p| p.name);
    match check::advice(error, provider) {
        Advice::CheckPassword => {
            gettext("The password was not accepted. Please check it and try again.")
        }
        Advice::UseAppPassword => fmt(
            &gettext(
                "The app password was not accepted. Make sure you enter the app password, not your normal {} password.",
            ),
            &[name],
        ),
        Advice::CheckPasswordAndAccess => fmt(
            &gettext(
                "Signing in did not work. Check your password and whether access is really allowed at {}.",
            ),
            &[name],
        ),
        Advice::ImapUnreachable => fmt(
            &gettext(
                "The server for incoming mail ({}) could not be reached. Check your internet connection and the server details.",
            ),
            &[&settings.imap.host],
        ),
        Advice::SmtpUnreachable => fmt(
            &gettext(
                "Receiving mail works, but the server for outgoing mail ({}) could not be reached.",
            ),
            &[&settings.smtp.host],
        ),
        Advice::MailboxFailed => gettext(
            "Signing in worked, but the mailbox could not be opened. Please try again later.",
        ),
    }
}

trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&Self)) -> Self {
        f(&self);
        self
    }
}

impl<T> Tap for T {}

fn start_discovery(email: String) -> DiscoveryFuture {
    async move {
        runtime::run(async move {
            let net = RealNetwork::new(Trust::system());
            match tokio::time::timeout(
                std::time::Duration::from_secs(40),
                discovery::discover(&email, &net),
            )
            .await
            {
                Ok(found) => found,
                Err(_) => Discovery::NotFound,
            }
        })
        .await
    }
    .boxed_local()
    .shared()
}

fn email_status(email: &str) -> gtk::Widget {
    let email = email.trim();
    let Some((_, domain)) = providers::split_address(email) else {
        // Plain text: the names are examples, not buttons.
        return gtk::Label::builder()
            .label(gettext(
                "Works with Posteo, GMX, WEB.DE, Gmail, iCloud, mailbox.org and many more, \
                 or with your own server.",
            ))
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build()
            .upcast();
    };
    match providers::provider_for_domain(&domain) {
        Some(provider) if provider.preparation == Preparation::Unsupported => widgets::card(
            "M",
            "Microsoft",
            &gettext("Unfortunately not supported"),
            Some("dialog-warning-symbolic"),
            Some("warning"),
        ),
        Some(provider) => widgets::card(
            provider.name,
            provider.name,
            &if provider.preparation == Preparation::None {
                gettext("Settings are known")
            } else {
                gettext("Settings are known · one short extra step")
            },
            Some("object-select-symbolic"),
            Some("success"),
        ),
        None => widgets::card(
            &domain,
            &domain,
            &gettext("Settings will be looked up in the next step"),
            Some("system-search-symbolic"),
            None,
        ),
    }
    .upcast()
}

fn preparation_steps(provider: ProviderId) -> Vec<String> {
    match provider {
        ProviderId::Gmail => vec![
            gettext("Open your Google account and go to “Security”."),
            gettext("Turn on 2-Step Verification if it is still off."),
            gettext("Under “App passwords”, create a new password, for example named “Mail”."),
        ],
        ProviderId::ICloud => vec![
            gettext("Sign in at account.apple.com."),
            gettext("Open “Sign-In and Security” and then “App-Specific Passwords”."),
            gettext("Create a new password, for example named “Mail”."),
        ],
        ProviderId::Yahoo => vec![
            gettext("Sign in to Yahoo and open “Account security”."),
            gettext("Choose “Generate app password”."),
            gettext("Enter “Mail” as the name and copy the password shown."),
        ],
        ProviderId::Gmx => vec![
            gettext("Sign in at gmx.net."),
            gettext("Open “Email settings” and then “POP3/IMAP retrieval”."),
            gettext("Tick “Allow access via POP3 and IMAP” and save."),
        ],
        ProviderId::WebDe => vec![
            gettext("Sign in at web.de."),
            gettext("Open “Settings” and then “POP3/IMAP retrieval”."),
            gettext("Tick “Allow access via POP3 and IMAP” and save."),
        ],
        _ => Vec::new(),
    }
}

fn default_settings(email: &str) -> ServerSettings {
    let domain = providers::split_address(email)
        .map(|(_, domain)| domain)
        .unwrap_or_default();
    ServerSettings {
        imap: ServerConfig::new(format!("imap.{domain}"), 993, Security::Tls),
        smtp: ServerConfig::new(format!("smtp.{domain}"), 465, Security::Tls),
        username: email.trim().to_owned(),
    }
}

/// "max.berger@…" becomes "Max Berger".
pub fn guess_name(email: &str) -> String {
    let local = email.split('@').next().unwrap_or_default();
    local
        .split(['.', '_', '-', '+'])
        .filter(|word| !word.is_empty() && !word.chars().all(|c| c.is_ascii_digit()))
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The provider's name, or the first part of the domain.
pub fn guess_label(email: &str, provider: Option<&Provider>) -> String {
    if let Some(provider) = provider {
        return provider.name.to_owned();
    }
    let domain = email.rsplit('@').next().unwrap_or_default();
    let first = domain.split('.').next().unwrap_or_default();
    let mut chars = first.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// Server name, port and encryption of one server, as rows of a group.
pub struct ServerFields {
    pub group: adw::PreferencesGroup,
    pub host: adw::EntryRow,
    pub port: adw::SpinRow,
    pub security: adw::ComboRow,
}

impl ServerFields {
    pub fn new(title: &str, config: &ServerConfig) -> ServerFields {
        let host = adw::EntryRow::builder()
            .title(gettext("Server"))
            .text(config.host.as_str())
            .input_purpose(gtk::InputPurpose::Url)
            .build();
        let port = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
        port.set_title(&gettext("Port"));
        port.set_value(f64::from(config.port));
        let security = adw::ComboRow::builder()
            .title(gettext("Encryption"))
            .model(&gtk::StringList::new(&[
                &gettext("SSL/TLS (recommended)"),
                &gettext("STARTTLS"),
            ]))
            .build();
        security.set_selected(match config.security {
            Security::Tls => 0,
            Security::StartTls => 1,
        });
        {
            let security = security.clone();
            port.connect_value_notify(move |port| match Security::for_port(port.value() as u16) {
                Some(Security::Tls) => security.set_selected(0),
                Some(Security::StartTls) => security.set_selected(1),
                None => {}
            });
        }
        let group = adw::PreferencesGroup::builder().title(title).build();
        group.add(&host);
        group.add(&port);
        group.add(&security);
        ServerFields {
            group,
            host,
            port,
            security,
        }
    }

    pub fn config(&self) -> ServerConfig {
        ServerConfig {
            host: self.host.text().trim().to_owned(),
            port: self.port.value() as u16,
            security: if self.security.selected() == 1 {
                Security::StartTls
            } else {
                Security::Tls
            },
        }
    }
}

fn build_rail() -> (gtk::Box, Rail) {
    let rail = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(26)
        .width_request(250)
        .hexpand(false)
        .css_classes(["setup-rail"])
        .build();
    let intro = gtk::Box::new(gtk::Orientation::Vertical, 2);
    intro.append(
        &gtk::Label::builder()
            .label(gettext("New Account"))
            .xalign(0.0)
            .css_classes(["heading"])
            .build(),
    );
    intro.append(
        &gtk::Label::builder()
            .label(gettext("Takes about a minute."))
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    rail.append(&intro);

    let titles = [
        gettext("Email address"),
        gettext("Sign in"),
        gettext("Connect"),
        gettext("Finish"),
    ];
    let stepper = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let mut dots = Vec::new();
    let mut rows = Vec::new();
    let mut subtitles = Vec::new();
    let mut segments = Vec::new();
    for (i, title) in titles.iter().enumerate() {
        let number = gtk::Label::new(Some(&(i + 1).to_string()));
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        let dot = gtk::Stack::builder()
            .valign(gtk::Align::Start)
            .halign(gtk::Align::Center)
            .css_classes(["setup-dot"])
            .build();
        dot.add_named(&number, Some("number"));
        dot.add_named(&check, Some("done"));
        let subtitle = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_valign(gtk::Align::Center);
        text.append(
            &gtk::Label::builder()
                .label(title)
                .xalign(0.0)
                .css_classes(["heading"])
                .build(),
        );
        text.append(&subtitle);
        let row = gtk::Box::builder()
            .spacing(12)
            .css_classes(["setup-step"])
            .build();
        row.append(&dot);
        row.append(&text);
        stepper.append(&row);
        dots.push((dot, number));
        rows.push(row);
        subtitles.push(subtitle);
        segments.push(
            gtk::Box::builder()
                .hexpand(true)
                .css_classes(["setup-segment"])
                .build(),
        );
    }
    rail.append(&stepper);

    let foot = widgets::note(
        "channel-secure-symbolic",
        &gettext("Your login details stay on this device, in the keyring."),
    );
    foot.set_vexpand(true);
    foot.set_valign(gtk::Align::End);
    rail.append(&foot);
    (
        rail,
        Rail {
            dots,
            rows,
            subtitles,
            segments,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_names_from_the_address() {
        assert_eq!(guess_name("max.berger@example.org"), "Max Berger");
        assert_eq!(
            guess_name("anna_lena-mueller2@gmx.de"),
            "Anna Lena Mueller2"
        );
        assert_eq!(guess_name("info+news@example.org"), "Info News");
        assert_eq!(guess_name("1234@example.org"), "");
        assert_eq!(
            guess_label("a@gmx.de", providers::provider_for_address("a@gmx.de")),
            "GMX"
        );
        assert_eq!(guess_label("a@sportverein.de", None), "Sportverein");
    }
}
