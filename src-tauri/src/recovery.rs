use crate::{
    config::ConfigStore,
    discord::{
        self, BridgeStatus, ConnectFailure, DiscordCommand, RpcState, VoiceControl, VoiceState,
    },
};
use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const IDLE_POLL: Duration = Duration::from_secs(1);
const HEARTBEAT: Duration = Duration::from_secs(15);
const WARN_AFTER: Duration = Duration::from_secs(60);

pub(crate) trait Connection: VoiceControl + Send {
    fn account_id(&self) -> Option<&str>;
    fn broken(&self) -> bool;
    fn poll_idle(&mut self) -> Result<(), String>;
    fn heartbeat(&mut self) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    Unavailable,
    AuthorizationRequired,
    VoiceStateUncertain,
    VoiceStateUnavailable,
}

impl Notice {
    pub(crate) fn body(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "Discord is disconnected. Automatic mute/deafen is not guaranteed. Open Discord or check bridge settings; background reconnects will continue."
            }
            Self::AuthorizationRequired => {
                "Discord authorization needs attention. Open bridge settings and click Connect. Automatic mute/deafen is unavailable until connected."
            }
            Self::VoiceStateUncertain => {
                "The Discord account or app settings changed during recovery. Check your mute/deafen state; the bridge did not restore the old account's state."
            }
            Self::VoiceStateUnavailable => {
                "Discord did not provide your starting voice state. Check mute/deafen manually after dictation; the bridge will not guess that unmuting is safe."
            }
        }
    }
}

#[derive(Default)]
struct Retry {
    next: Option<Instant>,
    since: Option<Instant>,
    failures: u32,
    paused: bool,
    notified: bool,
    last_notice: Option<Notice>,
}

impl Retry {
    fn start(&mut self, now: Instant) {
        *self = Self {
            next: Some(now),
            ..Default::default()
        };
    }

    fn failed(&mut self, now: Instant, needs_action: bool) {
        self.since.get_or_insert(now);
        self.failures = self.failures.saturating_add(1);
        self.paused = needs_action;
        let seconds = (1_u64 << self.failures.saturating_sub(1).min(5)).min(30);
        self.next = (!needs_action).then_some(now + Duration::from_secs(seconds));
    }

    fn recovered(&mut self) {
        *self = Self::default();
    }

    fn due(&self, now: Instant) -> bool {
        self.next.is_some_and(|deadline| now >= deadline)
    }

    fn warning_due(&self, now: Instant) -> bool {
        !self.notified
            && self
                .since
                .is_some_and(|start| now.saturating_duration_since(start) >= WARN_AFTER)
    }
}

struct Session {
    prior: Option<VoiceState>,
    applied: Option<VoiceState>,
    account_id: String,
    client_id: String,
}

// Remember the exact attempted write, even if its acknowledgement is lost.
// In particular, an announcement's initial mute has not yet applied deafen.
struct TrackedVoice<'a, C> {
    rpc: &'a mut C,
    applied: &'a mut Option<VoiceState>,
}

impl<C: Connection> TrackedVoice<'_, C> {
    fn record(&mut self, mute: bool, deaf: Option<bool>) -> Result<(), String> {
        if self.rpc.broken() {
            return Err("Discord pipe is disconnected".into());
        }
        if let Some(state) = self.applied.as_mut() {
            state.mute = mute;
            if let Some(deaf) = deaf {
                state.deaf = deaf;
            }
        }
        Ok(())
    }
}

impl<C: Connection> VoiceControl for TrackedVoice<'_, C> {
    fn get_voice_settings(&mut self) -> Result<VoiceState, String> {
        self.rpc.get_voice_settings()
    }
    fn soundboard_allowed(&mut self, guilds: &[String]) -> Result<bool, String> {
        self.rpc.soundboard_allowed(guilds)
    }
    fn set_voice_settings(&mut self, mute: bool, deaf: Option<bool>) -> Result<(), String> {
        self.record(mute, deaf)?;
        self.rpc.set_voice_settings(mute, deaf)
    }
    fn play_soundboard_sound(&mut self, sound: &str) -> Result<(), String> {
        self.rpc.play_soundboard_sound(sound)
    }
    fn mute_and_play(&mut self, deaf: Option<bool>, sound: &str) -> Result<Option<String>, String> {
        self.record(true, deaf)?;
        self.rpc.mute_and_play(deaf, sound)
    }
}

type StatusCallback = Box<dyn Fn(BridgeStatus, Option<Notice>) + Send>;

struct Worker<C: Connection> {
    store: Arc<ConfigStore>,
    status: Arc<Mutex<BridgeStatus>>,
    on_status: StatusCallback,
    credentials: (String, String),
    client: Option<C>,
    connected_at: Option<Instant>,
    session: Option<Session>,
    wanted: bool,
    retry: Retry,
    pending_deafen: Option<Instant>,
    last_sound_attempt: Option<Instant>,
    unknown_notified: bool,
    next_poll: Instant,
    next_heartbeat: Instant,
}

impl<C: Connection> Worker<C> {
    fn new(
        store: Arc<ConfigStore>,
        status: Arc<Mutex<BridgeStatus>>,
        on_status: StatusCallback,
        now: Instant,
    ) -> Self {
        let config = store.get();
        let credentials = (
            config.discord_rpc.client_id,
            config.discord_rpc.client_secret,
        );
        let mut retry = Retry::default();
        if !credentials.0.is_empty() && !credentials.1.is_empty() {
            retry.start(now);
        }
        Self {
            store,
            status,
            on_status,
            credentials,
            client: None,
            connected_at: None,
            session: None,
            wanted: false,
            retry,
            pending_deafen: None,
            last_sound_attempt: None,
            unknown_notified: false,
            next_poll: now,
            next_heartbeat: now,
        }
    }

    fn publish(&self, rpc: RpcState, error: Option<String>) {
        #[cfg(debug_assertions)]
        eprintln!(
            "{} rpc state={rpc:?} error={}",
            discord::now_ms(),
            error.is_some()
        );
        let snapshot = self.status.lock().ok().map(|mut status| {
            status.rpc = rpc;
            status.rpc_error = error;
            status.clone()
        });
        if let Some(snapshot) = snapshot {
            (self.on_status)(snapshot, None);
        }
    }

    fn notice(&mut self, notice: Notice) {
        if self.retry.notified
            && (self.retry.last_notice != Some(Notice::Unavailable)
                || notice == Notice::Unavailable)
        {
            return;
        }
        // A transient outage warning may escalate once to an actionable warning,
        // but repeated automatic attempts must never repeat either notification.
        self.retry.notified = true;
        self.retry.last_notice = Some(notice);
        let snapshot = self.status.lock().ok().map(|status| status.clone());
        if let Some(snapshot) = snapshot {
            (self.on_status)(snapshot, Some(notice));
        }
    }

    fn active_now(&self) -> bool {
        self.status
            .lock()
            .map(|status| status.active)
            .unwrap_or(self.wanted)
    }

    fn failed(&mut self, error: String, needs_action: bool, now: Instant) {
        // Keep the original snapshot and ownership even when a write's reply is
        // lost. A later release must still be able to restore this session.
        self.client = None;
        self.connected_at = None;
        self.pending_deafen = None;
        self.retry.failed(now, needs_action);
        self.publish(
            if needs_action {
                RpcState::Disconnected
            } else {
                RpcState::Reconnecting
            },
            Some(error),
        );
        if needs_action {
            self.notice(Notice::AuthorizationRequired);
        } else if self.wanted || self.active_now() || self.retry.warning_due(now) {
            self.notice(Notice::Unavailable);
        }
    }

    fn connect(
        &mut self,
        interactive: bool,
        now: Instant,
        connector: &mut impl FnMut(bool) -> Result<C, ConnectFailure>,
    ) {
        self.publish(
            if interactive {
                RpcState::Connecting
            } else {
                RpcState::Reconnecting
            },
            None,
        );
        match connector(interactive) {
            Ok(client) => {
                self.client = Some(client);
                self.pending_deafen = None;
                // Input can change while authentication is in flight. Never mute
                // for an already-released gesture, or replay its announcement.
                self.wanted = self.active_now();
                let completed = Instant::now().max(now);
                match self.apply_voice(false, completed) {
                    Ok(error) => {
                        self.connected_at = Some(Instant::now().max(completed));
                        self.retry.recovered();
                        self.next_poll = completed + IDLE_POLL;
                        self.next_heartbeat = completed + HEARTBEAT;
                        self.publish(RpcState::Connected, error);
                    }
                    Err(error) => self.failed(error, false, completed),
                }
            }
            Err(error) => self.failed(error.message, error.needs_action, Instant::now().max(now)),
        }
    }

    fn apply_voice(&mut self, announce: bool, now: Instant) -> Result<Option<String>, String> {
        let mut warning = None;
        if let Some(session) = &self.session {
            let client = self.client.as_ref().expect("connected");
            if client.account_id() != Some(session.account_id.as_str())
                || self.credentials.0 != session.client_id
            {
                // A snapshot from another account/app must never open this mic.
                self.session = None;
                self.notice(Notice::VoiceStateUncertain);
                warning =
                    Some("Discord account or app changed. Check your mute/deafen state.".into());
            }
        }
        let rpc = self.client.as_mut().expect("connected");
        if !self.wanted {
            self.pending_deafen = None;
            if let Some(session) = self.session.as_mut() {
                if session.prior.is_none() {
                    self.session = None;
                    if !self.unknown_notified {
                        self.unknown_notified = true;
                        self.notice(Notice::VoiceStateUnavailable);
                    }
                    return Ok(Some(
                        "Starting voice state was unavailable. Check mute/deafen manually.".into(),
                    ));
                }
                discord::preserve_manual_voice(rpc, &mut session.prior, session.applied)?;
                if let Some(prior) = session.prior {
                    // Keep the last owned restriction until restoration is acknowledged;
                    // a lost reply may mean the restriction is still in place.
                    rpc.set_voice_settings(prior.mute, Some(prior.deaf))?;
                }
                self.session = None;
            }
            return Ok(warning);
        }
        let first = self.session.is_none();
        if first {
            let prior = match rpc.get_voice_settings() {
                Ok(state) => {
                    self.unknown_notified = false;
                    Some(state)
                }
                Err(error) if rpc.broken() => return Err(error),
                Err(_) => {
                    warning = Some("Starting voice state is unavailable. Mute/deafen must be checked manually after dictation.".into());
                    None
                }
            };
            if !self
                .status
                .lock()
                .map(|status| status.active)
                .unwrap_or(self.wanted)
            {
                self.wanted = false;
                return Ok(warning);
            }
            self.session = Some(Session {
                prior,
                applied: prior,
                account_id: rpc.account_id().ok_or("Discord account is unknown")?.into(),
                client_id: self.credentials.0.clone(),
            });
        }
        let session = self.session.as_mut().expect("session started");
        if !first {
            discord::preserve_manual_voice(rpc, &mut session.prior, session.applied)?;
        }
        let config = self.store.get();
        let can_play = announce
            && first
            && session.prior.is_some_and(|state| !state.deaf)
            && !config.soundboard_sound_id.is_empty()
            && self
                .last_sound_attempt
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(5));
        let deafen = config.deafen_while_active && self.pending_deafen.is_none();
        let mut tracked = TrackedVoice {
            rpc,
            applied: &mut session.applied,
        };
        if can_play {
            let previous_attempt = self.last_sound_attempt;
            self.last_sound_attempt = Some(now);
            let announcement = discord::begin_announcement(
                &mut tracked,
                session.prior,
                &config.soundboard_sound_id,
                config.deafen_while_active,
                &config.soundboard_guild_ids,
            )?;
            if !announcement.sound_attempted {
                self.last_sound_attempt = previous_attempt;
            }
            self.pending_deafen = announcement.deafen_at;
            Ok(announcement
                .sound_error
                .map(|error| format!("Soundboard: {error}"))
                .or(warning))
        } else {
            discord::set_active_voice(&mut tracked, session.prior, deafen)?;
            if session.prior.is_none() && !self.unknown_notified {
                self.unknown_notified = true;
                self.notice(Notice::VoiceStateUnavailable);
            }
            Ok(warning)
        }
    }

    fn command(
        &mut self,
        command: DiscordCommand,
        now: Instant,
        connector: &mut impl FnMut(bool) -> Result<C, ConnectFailure>,
    ) -> bool {
        match command {
            DiscordCommand::Connect => {
                self.retry.start(now);
                self.connect(true, now, connector);
            }
            DiscordCommand::SetMute(on) => {
                // Coalesce stale queued transitions after a slow RPC/auth request.
                if on != self.active_now() {
                    return true;
                }
                self.wanted = on;
                if !on {
                    self.pending_deafen = None;
                }
                if self.client.is_some() {
                    match self.apply_voice(true, now) {
                        Ok(error) => self.publish(RpcState::Connected, error),
                        Err(error) => self.failed(error, false, Instant::now().max(now)),
                    }
                } else if on {
                    if self.retry.next.is_none() && !self.retry.paused {
                        self.publish(
                            RpcState::Disconnected,
                            Some("Set up Discord credentials and click Connect.".into()),
                        );
                        self.notice(Notice::AuthorizationRequired);
                    } else {
                        self.notice(Notice::Unavailable);
                    }
                }
            }
            DiscordCommand::RefreshActive => {
                let config = self.store.get();
                let credentials = (
                    config.discord_rpc.client_id,
                    config.discord_rpc.client_secret,
                );
                if credentials != self.credentials {
                    self.credentials = credentials;
                    self.client = None;
                    self.pending_deafen = None;
                    self.retry = Retry::default();
                    if !self.credentials.0.is_empty() && !self.credentials.1.is_empty() {
                        self.retry.start(now);
                    } else {
                        self.publish(RpcState::Disconnected, None);
                    }
                } else if self.client.is_some() && self.session.is_some() {
                    if !config.deafen_while_active || config.soundboard_sound_id.is_empty() {
                        self.pending_deafen = None;
                    }
                    match self.apply_voice(false, now) {
                        Ok(error) => self.publish(RpcState::Connected, error),
                        Err(error) => self.failed(error, false, Instant::now().max(now)),
                    }
                }
            }
            DiscordCommand::PlaySound(sound_id, requested_at, reply) => {
                // An explicit sound is attempted once, never queued across outages.
                let result = if self.client.is_some()
                    && self.connected_at.is_some_and(|ready| requested_at < ready)
                {
                    Err("Sound skipped: Discord reconnected after this request".into())
                } else if now.saturating_duration_since(requested_at) >= Duration::from_secs(10) {
                    Err("Sound skipped: playback request expired".into())
                } else if let Some(rpc) = self.client.as_mut() {
                    discord::play_allowed_sound(
                        rpc,
                        &sound_id,
                        &self.store.get().soundboard_guild_ids,
                    )
                } else {
                    Err(if self.retry.paused {
                        "Discord authorization needs attention; click Connect in bridge settings"
                    } else if self.retry.next.is_some() {
                        "Discord is disconnected; reconnecting in the background"
                    } else {
                        "Discord is not configured; open bridge settings and click Connect"
                    }
                    .into())
                };
                if self.client.as_ref().is_some_and(Connection::broken) {
                    self.failed(
                        result
                            .as_ref()
                            .err()
                            .cloned()
                            .unwrap_or("Discord pipe disconnected".into()),
                        false,
                        Instant::now().max(now),
                    );
                } else if self.client.is_some() {
                    self.publish(
                        RpcState::Connected,
                        result
                            .as_ref()
                            .err()
                            .map(|error| format!("Soundboard: {error}")),
                    );
                }
                let _ = reply.try_send(result);
            }
            DiscordCommand::Shutdown => {
                if let (Some(rpc), Some(session)) = (self.client.as_mut(), self.session.as_ref()) {
                    if rpc.account_id() == Some(session.account_id.as_str())
                        && self.credentials.0 == session.client_id
                    {
                        let _ = discord::restore_voice(rpc, session.prior, session.applied);
                    }
                }
                return false;
            }
        }
        true
    }

    fn tick(
        &mut self,
        now: Instant,
        connector: &mut impl FnMut(bool) -> Result<C, ConnectFailure>,
    ) {
        if self.client.is_none() {
            if self.retry.due(now) {
                self.connect(false, now, connector);
            }
            if self.retry.warning_due(now) {
                self.notice(Notice::Unavailable);
            }
            return;
        }
        if self.pending_deafen.is_some_and(|deadline| now >= deadline) {
            self.pending_deafen = None;
            if self.wanted && self.active_now() {
                if let Err(error) = self.apply_voice(false, now) {
                    self.failed(error, false, Instant::now().max(now));
                    return;
                }
            }
        }
        if now >= self.next_poll {
            if let Err(error) = self.client.as_mut().expect("connected").poll_idle() {
                self.failed(error, false, Instant::now().max(now));
                return;
            }
            self.next_poll = Instant::now().max(now) + IDLE_POLL;
        }
        if now >= self.next_heartbeat {
            if let Err(error) = self.client.as_mut().expect("connected").heartbeat() {
                self.failed(error, false, Instant::now().max(now));
                return;
            }
            self.next_heartbeat = Instant::now().max(now) + HEARTBEAT;
        }
    }

    fn deadline(&self) -> Option<Instant> {
        if self.client.is_some() {
            [
                Some(self.next_poll),
                Some(self.next_heartbeat),
                self.pending_deafen,
            ]
            .into_iter()
            .flatten()
            .min()
        } else {
            let warning = if !self.retry.notified && !self.retry.paused {
                self.retry.since.map(|since| since + WARN_AFTER)
            } else {
                None
            };
            [self.retry.next, warning].into_iter().flatten().min()
        }
    }
}

pub(crate) fn start_worker(
    store: Arc<ConfigStore>,
    status: Arc<Mutex<BridgeStatus>>,
    on_status: impl Fn(BridgeStatus, Option<Notice>) + Send + 'static,
) -> mpsc::Sender<DiscordCommand> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("discord-rpc".into())
        .spawn(move || {
            let mut worker =
                Worker::new(store.clone(), status, Box::new(on_status), Instant::now());
            let mut connector = |interactive| discord::connect_authenticated(&store, interactive);
            let mut burst = 0;
            loop {
                // Bound bursts so skipped MIDI requests cannot starve pings.
                if burst >= 32 {
                    if worker.client.is_none() || worker.wanted == worker.active_now() {
                        worker.tick(Instant::now(), &mut connector);
                    }
                    burst = 0;
                }
                // Releases/quit take priority over optional idle health checks.
                match rx.try_recv() {
                    Ok(command) => {
                        if !worker.command(command, Instant::now(), &mut connector) {
                            break;
                        }
                        burst += 1;
                        continue;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        worker.command(DiscordCommand::Shutdown, Instant::now(), &mut connector);
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                worker.tick(Instant::now(), &mut connector);
                let received = match worker.deadline() {
                    Some(deadline) => {
                        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    }
                    None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                match received {
                    Ok(command) => {
                        if !worker.command(command, Instant::now(), &mut connector) {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        worker.command(DiscordCommand::Shutdown, Instant::now(), &mut connector);
                        break;
                    }
                }
            }
        })
        .expect("failed to start Discord worker");
    tx
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
