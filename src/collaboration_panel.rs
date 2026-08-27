//! Terminal projection for Zed channels and collaborative channel notes.
//!
//! The production path reads [`channel::ChannelStore`] and keeps channel-note
//! edits in [`channel::ChannelBuffer`].  This module owns only bounded terminal
//! selection/prompt state and the explicitly configured external media bridge.

use std::{
    collections::BTreeMap,
    env,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
};

use anyhow::{Context as _, Result, ensure};
use channel::{ChannelBuffer, ChannelStore};
use client::{Client, Collaborator, ParticipantIndex, Status, UserStore};
use collections::HashMap;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use gpui::{App, Entity, SharedString};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, Widget},
};
use rpc::proto::PeerId;
use serde::Deserialize;
use unicode_width::UnicodeWidthStr as _;

use crate::prompt::{LinePrompt, PromptAction};

pub(crate) struct ChannelNotesCollaborationHub(pub(crate) Entity<ChannelBuffer>);

impl editor::CollaborationHub for ChannelNotesCollaborationHub {
    fn collaborators<'a>(&self, cx: &'a App) -> &'a HashMap<PeerId, Collaborator> {
        self.0.read(cx).collaborators()
    }

    fn user_participant_indices<'a>(&self, cx: &'a App) -> &'a HashMap<u64, ParticipantIndex> {
        self.0.read(cx).user_store().read(cx).participant_indices()
    }

    fn user_names(&self, cx: &App) -> HashMap<u64, SharedString> {
        let user_ids = self.collaborators(cx).values().map(|user| user.user_id);
        self.0
            .read(cx)
            .user_store()
            .read(cx)
            .participant_names(user_ids, cx)
    }
}

pub(crate) const COLLABORATION_FIXTURE_ENV: &str = "ZEC_COLLABORATION_FIXTURE";
pub(crate) const MEDIA_BRIDGE_ENV: &str = "ZEC_MEDIA_BRIDGE";
const MAX_FIXTURE_BYTES: usize = 256 * 1024;
const MAX_CHANNELS: usize = 4096;
const MAX_COLLABORATORS: usize = 4096;
const MAX_MEDIA_ARGS: usize = 128;
const MAX_MEDIA_ENV: usize = 128;
const MAX_MEDIA_VALUE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MediaKind {
    Voice,
    Screen,
}

impl MediaKind {
    fn argument(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::Screen => "screen",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::Screen => "screen share",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CollaborationInput {
    Consumed,
    FocusEditor,
    OpenNotes(u64),
    Follow { channel_id: u64, user_id: u64 },
    SignIn,
    SignOut,
    Refresh,
    CreateChannel(String),
    RespondToInvite { channel_id: u64, accept: bool },
    ConfirmMedia(MediaKind),
    StopMedia(MediaKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionSection {
    Channels,
    Collaborators,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CollaborationChannel {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) depth: usize,
    pub(crate) invitation: bool,
    pub(crate) unread: bool,
    pub(crate) url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CollaborationParticipant {
    pub(crate) user_id: u64,
    pub(crate) username: String,
    pub(crate) online: bool,
    pub(crate) host: bool,
    pub(crate) replica_id: Option<u16>,
    pub(crate) fixture_row: Option<u32>,
    pub(crate) fixture_column: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fixture {
    #[serde(default = "default_fixture_account")]
    account: String,
    channels: Vec<FixtureChannel>,
}

fn default_fixture_account() -> String {
    "fixture-user".to_owned()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FixtureChannel {
    id: u64,
    name: String,
    #[serde(default)]
    depth: usize,
    #[serde(default)]
    invitation: bool,
    #[serde(default)]
    unread: bool,
    #[serde(default)]
    url: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    collaborators: Vec<FixtureCollaborator>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FixtureCollaborator {
    user_id: u64,
    username: String,
    #[serde(default = "default_true")]
    online: bool,
    #[serde(default)]
    host: bool,
    #[serde(default)]
    row: Option<u32>,
    #[serde(default)]
    column: Option<u32>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MediaBridgeConfig {
    command: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

impl MediaBridgeConfig {
    fn from_environment() -> Result<Option<Self>> {
        let Some(json) = env::var_os(MEDIA_BRIDGE_ENV) else {
            return Ok(None);
        };
        let json = json.to_string_lossy();
        ensure!(
            json.len() <= MAX_FIXTURE_BYTES,
            "{MEDIA_BRIDGE_ENV} exceeds {MAX_FIXTURE_BYTES} bytes"
        );
        let config: Self = serde_json::from_str(&json)
            .with_context(|| format!("parse {MEDIA_BRIDGE_ENV} as JSON"))?;
        ensure!(
            !config.command.as_os_str().is_empty(),
            "{MEDIA_BRIDGE_ENV}.command must not be empty"
        );
        ensure!(
            config.args.len() <= MAX_MEDIA_ARGS,
            "{MEDIA_BRIDGE_ENV}.args exceeds {MAX_MEDIA_ARGS} entries"
        );
        ensure!(
            config.env.len() <= MAX_MEDIA_ENV,
            "{MEDIA_BRIDGE_ENV}.env exceeds {MAX_MEDIA_ENV} entries"
        );
        ensure!(
            config
                .args
                .iter()
                .all(|value| { value.len() <= MAX_MEDIA_VALUE_BYTES && !value.contains('\0') }),
            "{MEDIA_BRIDGE_ENV}.args contains an oversized or NUL-bearing value"
        );
        ensure!(
            config.env.iter().all(|(name, value)| {
                !name.is_empty()
                    && name.len() <= MAX_MEDIA_VALUE_BYTES
                    && value.len() <= MAX_MEDIA_VALUE_BYTES
                    && !name.contains(['=', '\0'])
                    && !value.contains('\0')
            }),
            "{MEDIA_BRIDGE_ENV}.env contains an invalid name or value"
        );
        Ok(Some(config))
    }
}

#[derive(Default)]
struct MediaBridge {
    config: Option<MediaBridgeConfig>,
    voice: Option<Child>,
    screen: Option<Child>,
}

impl MediaBridge {
    fn new() -> (Self, Option<String>) {
        match MediaBridgeConfig::from_environment() {
            Ok(config) => (
                Self {
                    config,
                    voice: None,
                    screen: None,
                },
                None,
            ),
            Err(error) => (
                Self::default(),
                Some(format!("media bridge configuration rejected: {error:#}")),
            ),
        }
    }

    fn configured(&self) -> bool {
        self.config.is_some()
    }

    fn slot(&self, kind: MediaKind) -> &Option<Child> {
        match kind {
            MediaKind::Voice => &self.voice,
            MediaKind::Screen => &self.screen,
        }
    }

    fn slot_mut(&mut self, kind: MediaKind) -> &mut Option<Child> {
        match kind {
            MediaKind::Voice => &mut self.voice,
            MediaKind::Screen => &mut self.screen,
        }
    }

    fn running(&self, kind: MediaKind) -> bool {
        self.slot(kind).is_some()
    }

    fn poll(&mut self) -> Vec<String> {
        let mut notices = Vec::new();
        for kind in [MediaKind::Voice, MediaKind::Screen] {
            let finished = match self.slot_mut(kind).as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => {
                        Some(format!("{} bridge exited with {status}", kind.label()))
                    }
                    Ok(None) => None,
                    Err(error) => Some(format!("{} bridge status failed: {error}", kind.label())),
                },
                None => None,
            };
            if let Some(notice) = finished {
                self.slot_mut(kind).take();
                notices.push(notice);
            }
        }
        notices
    }

    fn start(&mut self, kind: MediaKind, channel_url: &str) -> Result<()> {
        ensure!(
            !self.running(kind),
            "{} bridge is already active",
            kind.label()
        );
        let config = self
            .config
            .as_ref()
            .with_context(|| format!("{MEDIA_BRIDGE_ENV} is not configured"))?;
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .arg(kind.argument())
            .arg(channel_url)
            .envs(&config.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().with_context(|| {
            format!("start {} bridge {}", kind.label(), config.command.display())
        })?;
        *self.slot_mut(kind) = Some(child);
        Ok(())
    }

    fn stop(&mut self, kind: MediaKind) -> Result<bool> {
        let Some(mut child) = self.slot_mut(kind).take() else {
            return Ok(false);
        };
        if child.try_wait()?.is_none() {
            child
                .kill()
                .with_context(|| format!("terminate owned {} bridge process", kind.label()))?;
        }
        let _ = child.wait();
        Ok(true)
    }
}

impl Drop for MediaBridge {
    fn drop(&mut self) {
        let _ = self.stop(MediaKind::Voice);
        let _ = self.stop(MediaKind::Screen);
    }
}

pub(crate) struct CollaborationPanelState {
    fixture: Option<Fixture>,
    channels: Vec<CollaborationChannel>,
    collaborators: Vec<CollaborationParticipant>,
    contacts: Vec<String>,
    selected_channel: usize,
    selected_collaborator: usize,
    section: SelectionSection,
    account: String,
    connection: String,
    fixture_signed_in: bool,
    active_notes_channel: Option<u64>,
    create_prompt: Option<LinePrompt>,
    pending_media: Option<MediaKind>,
    media: MediaBridge,
    notice: Option<String>,
}

impl CollaborationPanelState {
    pub(crate) fn from_environment() -> Result<Self> {
        let fixture = env::var_os(COLLABORATION_FIXTURE_ENV)
            .map(|json| {
                let json = json.to_string_lossy();
                ensure!(
                    json.len() <= MAX_FIXTURE_BYTES,
                    "{COLLABORATION_FIXTURE_ENV} exceeds {MAX_FIXTURE_BYTES} bytes"
                );
                let fixture: Fixture = serde_json::from_str(&json)
                    .with_context(|| format!("parse {COLLABORATION_FIXTURE_ENV} as JSON"))?;
                validate_fixture(&fixture)?;
                Ok(fixture)
            })
            .transpose()?;
        let fixture_signed_in = fixture.is_some();
        let (media, media_notice) = MediaBridge::new();
        let mut state = Self {
            fixture,
            channels: Vec::new(),
            collaborators: Vec::new(),
            contacts: Vec::new(),
            selected_channel: 0,
            selected_collaborator: 0,
            section: SelectionSection::Channels,
            account: "signed out".to_owned(),
            connection: "signed out".to_owned(),
            fixture_signed_in,
            active_notes_channel: None,
            create_prompt: None,
            pending_media: None,
            media,
            notice: media_notice,
        };
        state.refresh_fixture();
        Ok(state)
    }

    pub(crate) fn unavailable(error: impl Into<String>) -> Self {
        let (media, media_notice) = MediaBridge::new();
        let mut notices = vec![error.into()];
        if let Some(notice) = media_notice {
            notices.push(notice);
        }
        Self {
            fixture: None,
            channels: Vec::new(),
            collaborators: Vec::new(),
            contacts: Vec::new(),
            selected_channel: 0,
            selected_collaborator: 0,
            section: SelectionSection::Channels,
            account: "signed out".to_owned(),
            connection: "configuration error".to_owned(),
            fixture_signed_in: false,
            active_notes_channel: None,
            create_prompt: None,
            pending_media: None,
            media,
            notice: Some(notices.join("; ")),
        }
    }

    pub(crate) fn is_fixture(&self) -> bool {
        self.fixture.is_some()
    }

    pub(crate) fn selected_channel_id(&self) -> Option<u64> {
        self.channels
            .get(self.selected_channel)
            .map(|channel| channel.id)
    }

    pub(crate) fn selected_channel_name(&self) -> Option<&str> {
        self.channels
            .get(self.selected_channel)
            .map(|channel| channel.name.as_str())
    }

    pub(crate) fn selected_channel_url(&self) -> Option<&str> {
        self.channels
            .get(self.selected_channel)
            .map(|channel| channel.url.as_str())
    }

    pub(crate) fn fixture_notes(&self, channel_id: u64) -> Option<String> {
        self.fixture
            .as_ref()?
            .channels
            .iter()
            .find_map(|channel| (channel.id == channel_id).then(|| channel.notes.clone()))
    }

    pub(crate) fn fixture_follow_point(&self, channel_id: u64, user_id: u64) -> Option<(u32, u32)> {
        let channel = self
            .fixture
            .as_ref()?
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)?;
        let collaborator = channel
            .collaborators
            .iter()
            .find(|collaborator| collaborator.user_id == user_id)?;
        Some((collaborator.row?, collaborator.column.unwrap_or(0)))
    }

    pub(crate) fn refresh(
        &mut self,
        client: &Arc<Client>,
        user_store: &Entity<UserStore>,
        channel_store: &Entity<ChannelStore>,
        active_notes: Option<(u64, Option<&Entity<ChannelBuffer>>)>,
        cx: &gpui::AsyncApp,
    ) {
        if let Some(notice) = self.media.poll().into_iter().last() {
            self.notice = Some(notice);
        }
        self.active_notes_channel = active_notes.map(|(channel_id, _)| channel_id);
        if self.fixture.is_some() {
            self.refresh_fixture();
            return;
        }

        let selected_id = self.selected_channel_id();
        let (connection, account, channels, contacts, collaborators) = cx.update(|cx| {
            let status = *client.status().borrow();
            let connection = status_label(status).to_owned();
            let account = user_store
                .read(cx)
                .current_user()
                .map(|user| format!("@{}", user.username))
                .unwrap_or_else(|| "signed out".to_owned());
            let store = channel_store.read(cx);
            let mut channels = store
                .ordered_channels()
                .map(|(depth, channel)| CollaborationChannel {
                    id: channel.id.0,
                    name: channel.name.to_string(),
                    depth,
                    invitation: store.has_channel_invitation(channel.id),
                    unread: store.has_channel_buffer_changed(channel.id),
                    url: channel.link(cx),
                })
                .collect::<Vec<_>>();
            for channel in store.channel_invitations() {
                if channels.iter().all(|entry| entry.id != channel.id.0) {
                    channels.push(CollaborationChannel {
                        id: channel.id.0,
                        name: channel.name.to_string(),
                        depth: channel.parent_path.len(),
                        invitation: true,
                        unread: false,
                        url: channel.link(cx),
                    });
                }
            }
            let contacts = user_store
                .read(cx)
                .contacts()
                .iter()
                .map(|contact| {
                    format!(
                        "{} @{}",
                        if contact.online { "●" } else { "○" },
                        contact.user.username
                    )
                })
                .collect::<Vec<_>>();
            let collaborators = active_notes
                .and_then(|(_, notes)| notes)
                .map(|notes| {
                    notes
                        .read(cx)
                        .collaborators()
                        .values()
                        .map(|collaborator| {
                            let username = user_store
                                .read(cx)
                                .get_cached_user(collaborator.user_id)
                                .map(|user| user.username.to_string())
                                .unwrap_or_else(|| format!("user-{}", collaborator.user_id));
                            CollaborationParticipant {
                                user_id: collaborator.user_id,
                                username,
                                online: true,
                                host: collaborator.is_host,
                                replica_id: Some(collaborator.replica_id.as_u16()),
                                fixture_row: None,
                                fixture_column: None,
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (connection, account, channels, contacts, collaborators)
        });
        self.connection = connection;
        self.account = account;
        self.channels = channels;
        self.contacts = contacts;
        self.collaborators = collaborators;
        self.restore_selection(selected_id);
    }

    fn refresh_fixture(&mut self) {
        let Some(fixture) = self.fixture.clone() else {
            return;
        };
        let selected_id = self.selected_channel_id();
        self.connection = if self.fixture_signed_in {
            "fixture-connected"
        } else {
            "signed out"
        }
        .to_owned();
        self.account = if self.fixture_signed_in {
            format!("@{}", fixture.account)
        } else {
            "signed out".to_owned()
        };
        self.channels = fixture
            .channels
            .iter()
            .map(|channel| CollaborationChannel {
                id: channel.id,
                name: channel.name.clone(),
                depth: channel.depth,
                invitation: channel.invitation,
                unread: channel.unread,
                url: if channel.url.is_empty() {
                    format!("https://example.invalid/channel/{}", channel.id)
                } else {
                    channel.url.clone()
                },
            })
            .collect();
        self.restore_selection(selected_id);
        self.collaborators = self
            .selected_channel_id()
            .and_then(|selected| {
                fixture
                    .channels
                    .iter()
                    .find(|channel| channel.id == selected)
            })
            .map(|channel| {
                channel
                    .collaborators
                    .iter()
                    .map(|collaborator| CollaborationParticipant {
                        user_id: collaborator.user_id,
                        username: collaborator.username.clone(),
                        online: collaborator.online,
                        host: collaborator.host,
                        replica_id: None,
                        fixture_row: collaborator.row,
                        fixture_column: collaborator.column,
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.selected_collaborator = self
            .selected_collaborator
            .min(self.collaborators.len().saturating_sub(1));
    }

    fn restore_selection(&mut self, selected_id: Option<u64>) {
        self.selected_channel = selected_id
            .and_then(|selected| {
                self.channels
                    .iter()
                    .position(|channel| channel.id == selected)
            })
            .unwrap_or_else(|| {
                self.selected_channel
                    .min(self.channels.len().saturating_sub(1))
            });
        if self.collaborators.is_empty() {
            self.section = SelectionSection::Channels;
        }
    }

    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> CollaborationInput {
        if key.kind == KeyEventKind::Release {
            return CollaborationInput::Consumed;
        }
        if let Some(prompt) = self.create_prompt.as_mut() {
            return match prompt.handle_key(key) {
                PromptAction::Submit | PromptAction::AlternateSubmit => {
                    let name = prompt.text().trim().to_owned();
                    if name.is_empty() {
                        self.notice = Some("channel name must not be empty".to_owned());
                        CollaborationInput::Consumed
                    } else {
                        self.create_prompt = None;
                        CollaborationInput::CreateChannel(name)
                    }
                }
                PromptAction::Cancel => {
                    self.create_prompt = None;
                    CollaborationInput::Consumed
                }
                PromptAction::Changed => {
                    self.notice = None;
                    CollaborationInput::Consumed
                }
                PromptAction::CursorMoved
                | PromptAction::Next
                | PromptAction::Previous
                | PromptAction::Ignored => CollaborationInput::Consumed,
            };
        }
        if let Some(kind) = self.pending_media {
            return match (key.code, key.modifiers) {
                (KeyCode::Char('y' | 'Y'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    self.pending_media = None;
                    CollaborationInput::ConfirmMedia(kind)
                }
                (KeyCode::Char('n' | 'N'), KeyModifiers::NONE | KeyModifiers::SHIFT)
                | (KeyCode::Esc, KeyModifiers::NONE) => {
                    self.pending_media = None;
                    self.notice = Some(format!("{} bridge cancelled", kind.label()));
                    CollaborationInput::Consumed
                }
                _ => CollaborationInput::Consumed,
            };
        }
        if key.modifiers != KeyModifiers::NONE {
            return CollaborationInput::Consumed;
        }
        match key.code {
            KeyCode::Esc => CollaborationInput::FocusEditor,
            KeyCode::Up => {
                self.move_selection(-1);
                CollaborationInput::Consumed
            }
            KeyCode::Down => {
                self.move_selection(1);
                CollaborationInput::Consumed
            }
            KeyCode::Tab => {
                if !self.collaborators.is_empty() {
                    self.section = match self.section {
                        SelectionSection::Channels => SelectionSection::Collaborators,
                        SelectionSection::Collaborators => SelectionSection::Channels,
                    };
                }
                CollaborationInput::Consumed
            }
            KeyCode::Enter => self
                .selected_channel_id()
                .map(CollaborationInput::OpenNotes)
                .unwrap_or(CollaborationInput::Consumed),
            KeyCode::Char('f') => self
                .collaborators
                .get(self.selected_collaborator)
                .and_then(|collaborator| {
                    self.selected_channel_id()
                        .map(|channel_id| CollaborationInput::Follow {
                            channel_id,
                            user_id: collaborator.user_id,
                        })
                })
                .unwrap_or(CollaborationInput::Consumed),
            KeyCode::Char('i') => {
                if self.connection == "connected" || self.connection == "fixture-connected" {
                    CollaborationInput::SignOut
                } else {
                    CollaborationInput::SignIn
                }
            }
            KeyCode::Char('r') => CollaborationInput::Refresh,
            KeyCode::Char('c') => {
                self.create_prompt = Some(LinePrompt::new());
                self.notice = None;
                CollaborationInput::Consumed
            }
            KeyCode::Char('a') => self
                .channels
                .get(self.selected_channel)
                .filter(|channel| channel.invitation)
                .map(|channel| CollaborationInput::RespondToInvite {
                    channel_id: channel.id,
                    accept: true,
                })
                .unwrap_or(CollaborationInput::Consumed),
            KeyCode::Char('d') => self
                .channels
                .get(self.selected_channel)
                .filter(|channel| channel.invitation)
                .map(|channel| CollaborationInput::RespondToInvite {
                    channel_id: channel.id,
                    accept: false,
                })
                .unwrap_or(CollaborationInput::Consumed),
            KeyCode::Char('v') => self.toggle_media(MediaKind::Voice),
            KeyCode::Char('s') => self.toggle_media(MediaKind::Screen),
            _ => CollaborationInput::Consumed,
        }
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        if let Some(prompt) = self.create_prompt.as_mut() {
            let _ = prompt.handle_paste(text);
            self.notice = None;
        }
    }

    fn toggle_media(&mut self, kind: MediaKind) -> CollaborationInput {
        if self.media.running(kind) {
            CollaborationInput::StopMedia(kind)
        } else {
            self.pending_media = Some(kind);
            self.notice = Some(format!(
                "start external {} bridge for the selected channel? y confirm / n cancel",
                kind.label()
            ));
            CollaborationInput::Consumed
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let (selected, len) = match self.section {
            SelectionSection::Channels => (&mut self.selected_channel, self.channels.len()),
            SelectionSection::Collaborators => {
                (&mut self.selected_collaborator, self.collaborators.len())
            }
        };
        if len > 0 {
            *selected = selected.saturating_add_signed(delta).min(len - 1);
        }
        if self.section == SelectionSection::Channels {
            self.refresh_fixture();
        }
    }

    pub(crate) fn create_fixture_channel(&mut self, name: String) -> Result<u64> {
        let fixture = self
            .fixture
            .as_mut()
            .context("collaboration fixture is not active")?;
        let next_id = fixture
            .channels
            .iter()
            .map(|channel| channel.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .context("fixture channel ID exhausted")?;
        fixture.channels.push(FixtureChannel {
            id: next_id,
            name,
            depth: 0,
            invitation: false,
            unread: false,
            url: String::new(),
            notes: String::new(),
            collaborators: Vec::new(),
        });
        self.selected_channel = fixture.channels.len() - 1;
        self.refresh_fixture();
        Ok(next_id)
    }

    pub(crate) fn respond_to_fixture_invite(
        &mut self,
        channel_id: u64,
        accept: bool,
    ) -> Result<()> {
        let fixture = self
            .fixture
            .as_mut()
            .context("collaboration fixture is not active")?;
        let index = fixture
            .channels
            .iter()
            .position(|channel| channel.id == channel_id)
            .context("fixture invitation channel disappeared")?;
        ensure!(
            fixture.channels[index].invitation,
            "channel is not an invitation"
        );
        if accept {
            fixture.channels[index].invitation = false;
        } else {
            fixture.channels.remove(index);
        }
        self.refresh_fixture();
        Ok(())
    }

    pub(crate) fn set_fixture_signed_in(&mut self, signed_in: bool) {
        self.fixture_signed_in = signed_in;
        self.refresh_fixture();
    }

    pub(crate) fn start_media(
        &mut self,
        kind: MediaKind,
        external_media_available: bool,
    ) -> Result<String> {
        ensure!(
            external_media_available,
            "terminal environment reports no external-media capability; set ZEC_EXTERNAL_MEDIA=1 after configuring a desktop bridge"
        );
        let url = self
            .selected_channel_url()
            .context("select a channel before starting media")?
            .to_owned();
        self.media.start(kind, &url)?;
        let message = format!("external {} bridge started", kind.label());
        self.notice = Some(message.clone());
        Ok(message)
    }

    pub(crate) fn stop_media(&mut self, kind: MediaKind) -> Result<String> {
        let stopped = self.media.stop(kind)?;
        let message = if stopped {
            format!("external {} bridge stopped", kind.label())
        } else {
            format!("external {} bridge was not running", kind.label())
        };
        self.notice = Some(message.clone());
        Ok(message)
    }

    pub(crate) fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub(crate) fn media_configured(&self) -> bool {
        self.media.configured()
    }

    fn status_line(&self) -> (String, Option<usize>) {
        if let Some(prompt) = &self.create_prompt {
            let prefix = "Create channel: ";
            let cursor = prefix.width()
                + prompt
                    .text()
                    .get(..prompt.cursor())
                    .unwrap_or_default()
                    .width();
            return (
                format!("{prefix}{}  Enter create  Esc cancel", prompt.text()),
                Some(cursor),
            );
        }
        if let Some(kind) = self.pending_media {
            return (
                format!(
                    "Permission: launch external {} bridge? y confirm / n cancel",
                    kind.label()
                ),
                None,
            );
        }
        (
            self.notice.clone().unwrap_or_else(|| {
                "↑/↓ select  Enter notes  Tab people  f follow  c create  a/d invite  i sign-in/out  v voice  s screen  Esc editor"
                    .to_owned()
            }),
            None,
        )
    }
}

fn validate_fixture(fixture: &Fixture) -> Result<()> {
    ensure!(
        !fixture.account.trim().is_empty(),
        "fixture account must not be empty"
    );
    ensure!(
        fixture.channels.len() <= MAX_CHANNELS,
        "fixture exceeds {MAX_CHANNELS} channels"
    );
    let mut ids = std::collections::HashSet::new();
    for channel in &fixture.channels {
        ensure!(channel.id != 0, "fixture channel IDs must be non-zero");
        ensure!(
            ids.insert(channel.id),
            "duplicate fixture channel ID {}",
            channel.id
        );
        ensure!(
            !channel.name.trim().is_empty(),
            "fixture channel name is empty"
        );
        ensure!(
            channel.collaborators.len() <= MAX_COLLABORATORS,
            "fixture channel {} exceeds {MAX_COLLABORATORS} collaborators",
            channel.id
        );
    }
    Ok(())
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::SignedOut => "signed out",
        Status::UpgradeRequired => "upgrade required",
        Status::Authenticating => "authenticating",
        Status::Authenticated => "authenticated",
        Status::AuthenticationError => "authentication error",
        Status::Connecting => "connecting",
        Status::ConnectionError => "connection error",
        Status::Connected { .. } => "connected",
        Status::ConnectionLost => "connection lost",
        Status::Reauthenticating => "reauthenticating",
        Status::Reauthenticated => "reauthenticated",
        Status::Reconnecting => "reconnecting",
        Status::ReconnectionError { .. } => "reconnection error",
    }
}

pub(crate) struct CollaborationPanelWidget<'a> {
    state: &'a CollaborationPanelState,
    focused: bool,
}

impl<'a> CollaborationPanelWidget<'a> {
    pub(crate) fn new(state: &'a CollaborationPanelState, focused: bool) -> Self {
        Self { state, focused }
    }

    pub(crate) fn cursor_position(&self, area: Rect) -> Option<Position> {
        let (_, cursor) = self.state.status_line();
        let cursor = cursor?;
        let inner = Block::default().borders(Borders::ALL).inner(area);
        if inner.is_empty() {
            return None;
        }
        Some(Position::new(
            inner
                .x
                .saturating_add(u16::try_from(cursor).unwrap_or(u16::MAX)),
            inner.bottom().saturating_sub(1),
        ))
    }
}

impl Widget for CollaborationPanelWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.width < 3 || area.height < 3 {
            return;
        }
        Clear.render(area, buffer);
        let title = format!(
            " Collaboration · {} · {} ",
            self.state.connection, self.state.account
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if self.focused {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            });
        let inner = block.inner(area);
        block.render(area, buffer);
        if inner.is_empty() {
            return;
        }

        let status_y = inner.bottom().saturating_sub(1);
        let content_bottom = status_y;
        let mut y = inner.y;
        write_bounded(
            buffer,
            inner,
            y,
            "Channels",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        y = y.saturating_add(1);
        for (index, channel) in self.state.channels.iter().enumerate() {
            if y >= content_bottom {
                break;
            }
            let selected = self.state.section == SelectionSection::Channels
                && index == self.state.selected_channel;
            let prefix = if selected { "›" } else { " " };
            let invite = if channel.invitation { " [invite]" } else { "" };
            let unread = if channel.unread { " •" } else { "" };
            let active = if self.state.active_notes_channel == Some(channel.id) {
                " ✎"
            } else {
                ""
            };
            let indent = "  ".repeat(channel.depth.min(16));
            let text = format!("{prefix} {indent}#{}{invite}{unread}{active}", channel.name);
            write_bounded(
                buffer,
                inner,
                y,
                &text,
                if selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else if channel.invitation {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                },
            );
            y = y.saturating_add(1);
        }

        if y < content_bottom {
            write_bounded(
                buffer,
                inner,
                y,
                "Collaborators",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            );
            y = y.saturating_add(1);
        }
        for (index, collaborator) in self.state.collaborators.iter().enumerate() {
            if y >= content_bottom {
                break;
            }
            let selected = self.state.section == SelectionSection::Collaborators
                && index == self.state.selected_collaborator;
            let prefix = if selected { "›" } else { " " };
            let online = if collaborator.online { "●" } else { "○" };
            let host = if collaborator.host { " host" } else { "" };
            write_bounded(
                buffer,
                inner,
                y,
                &format!("{prefix} {online} @{}{host}", collaborator.username),
                if selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                },
            );
            y = y.saturating_add(1);
        }

        if y < content_bottom && !self.state.contacts.is_empty() {
            write_bounded(
                buffer,
                inner,
                y,
                &format!("Contacts: {}", self.state.contacts.join(", ")),
                Style::default().fg(Color::DarkGray),
            );
            y = y.saturating_add(1);
        }
        if y < content_bottom {
            let voice = if self.state.media.running(MediaKind::Voice) {
                "on"
            } else {
                "off"
            };
            let screen = if self.state.media.running(MediaKind::Screen) {
                "on"
            } else {
                "off"
            };
            let configured = if self.state.media_configured() {
                "configured"
            } else {
                "not configured"
            };
            write_bounded(
                buffer,
                inner,
                y,
                &format!("Media bridge: {configured}; voice {voice}; screen {screen}"),
                Style::default().fg(Color::Blue),
            );
        }

        let (status, _) = self.state.status_line();
        write_bounded(
            buffer,
            inner,
            status_y,
            &status,
            Style::default().fg(if self.state.pending_media.is_some() {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        );
    }
}

fn write_bounded(buffer: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    if y >= area.bottom() {
        return;
    }
    buffer.set_stringn(area.x, y, text, usize::from(area.width), style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_rejects_duplicate_channels() {
        let fixture = Fixture {
            account: "test".to_owned(),
            channels: vec![
                FixtureChannel {
                    id: 1,
                    name: "one".to_owned(),
                    depth: 0,
                    invitation: false,
                    unread: false,
                    url: String::new(),
                    notes: String::new(),
                    collaborators: Vec::new(),
                },
                FixtureChannel {
                    id: 1,
                    name: "two".to_owned(),
                    depth: 0,
                    invitation: false,
                    unread: false,
                    url: String::new(),
                    notes: String::new(),
                    collaborators: Vec::new(),
                },
            ],
        };
        assert!(validate_fixture(&fixture).is_err());
    }

    #[test]
    fn status_labels_cover_connection_states() {
        assert_eq!(status_label(Status::SignedOut), "signed out");
        assert_eq!(status_label(Status::Authenticated), "authenticated");
    }
}
