use crate::{
    config::{ConfigStore, DictationApp, Mode},
    discord::{BridgeStatus, DiscordCommand},
    input::InputEvent,
};
use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct Gesture {
    active: bool,
    latched: bool,
    stopping: bool,
    down_at: Option<Instant>,
    second_deadline: Option<Instant>,
}

impl Gesture {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn handle(&mut self, event: InputEvent, mode: &Mode, app: &DictationApp, now: Instant) {
        match event {
            InputEvent::Dismiss => self.reset(),
            InputEvent::HandsFreePress => {
                if self.latched {
                    self.reset();
                } else {
                    self.active = true;
                    self.latched = true;
                    self.stopping = false;
                    self.down_at = None;
                    self.second_deadline = None;
                }
            }
            InputEvent::Press => {
                if self.latched {
                    self.stopping = true;
                    return;
                }
                match mode {
                    Mode::Hold => self.active = true,
                    Mode::Toggle => self.active = !self.active,
                    Mode::Auto => {
                        if self.second_deadline.is_some_and(|deadline| now <= deadline) {
                            self.latched = true;
                        }
                        self.second_deadline = None;
                        self.down_at = Some(now);
                        self.active = true;
                    }
                }
            }
            InputEvent::Release => {
                if self.latched {
                    if self.stopping {
                        self.reset();
                    }
                    return;
                }
                match mode {
                    Mode::Hold => self.active = false,
                    Mode::Toggle => {}
                    Mode::Auto => {
                        let Some(start) = self.down_at.take() else {
                            return;
                        };
                        if now.duration_since(start) > Duration::from_millis(250) {
                            self.reset();
                        } else {
                            self.second_deadline = Some(match app {
                                DictationApp::Willow => now + Duration::from_millis(350),
                                DictationApp::Wispr => start + Duration::from_millis(500),
                            });
                        }
                    }
                }
            }
        }
    }
}

pub fn start_gesture_worker(
    rx: mpsc::Receiver<InputEvent>,
    discord: mpsc::Sender<DiscordCommand>,
    store: Arc<ConfigStore>,
    status: Arc<Mutex<BridgeStatus>>,
) {
    thread::Builder::new()
        .name("dictation-gesture".into())
        .spawn(move || {
            let mut gesture = Gesture::default();
            loop {
                let received = match gesture.second_deadline {
                    Some(deadline) => {
                        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    }
                    None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                let was_active = gesture.active;
                let disconnected = matches!(received, Err(mpsc::RecvTimeoutError::Disconnected));
                let config = store.get();
                match received {
                    Ok(event) => {
                        gesture.handle(event, &config.mode, &config.dictation_app, Instant::now())
                    }
                    Err(_) => gesture.reset(),
                }
                if gesture.active != was_active {
                    if let Ok(mut status) = status.lock() {
                        status.active = gesture.active;
                    }
                    if !gesture.active && !disconnected && config.unmute_delay_ms > 0 {
                        thread::sleep(Duration::from_millis(config.unmute_delay_ms));
                    }
                    #[cfg(debug_assertions)]
                    eprintln!(
                        "{} gesture active={} latched={}",
                        crate::discord::now_ms(),
                        gesture.active,
                        gesture.latched
                    );
                    let _ = discord.send(DiscordCommand::SetMute(gesture.active));
                }
                if disconnected {
                    break;
                }
            }
        })
        .expect("failed to start gesture worker");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_mutes_until_release_for_both_apps() {
        for app in [DictationApp::Willow, DictationApp::Wispr] {
            let mut gesture = Gesture::default();
            let now = Instant::now();
            gesture.handle(InputEvent::Press, &Mode::Auto, &app, now);
            assert!(gesture.active);
            gesture.handle(
                InputEvent::Release,
                &Mode::Auto,
                &app,
                now + Duration::from_secs(1),
            );
            assert!(!gesture.active);
        }
    }

    #[test]
    fn double_tap_latches_and_next_tap_stops_for_both_apps() {
        for app in [DictationApp::Willow, DictationApp::Wispr] {
            let mut gesture = Gesture::default();
            let now = Instant::now();
            for (event, ms) in [
                (InputEvent::Press, 0),
                (InputEvent::Release, 50),
                (InputEvent::Press, 150),
                (InputEvent::Release, 200),
            ] {
                gesture.handle(event, &Mode::Auto, &app, now + Duration::from_millis(ms));
            }
            assert!(gesture.active && gesture.latched);
            assert!(gesture.second_deadline.is_none());
            gesture.handle(
                InputEvent::Press,
                &Mode::Auto,
                &app,
                now + Duration::from_secs(1),
            );
            gesture.handle(
                InputEvent::Release,
                &Mode::Auto,
                &app,
                now + Duration::from_secs(2),
            );
            assert!(!gesture.active);
        }
    }

    #[test]
    fn hands_free_promotes_hold_and_survives_key_release() {
        let mut gesture = Gesture::default();
        let now = Instant::now();
        gesture.handle(InputEvent::Press, &Mode::Auto, &DictationApp::Wispr, now);
        gesture.handle(
            InputEvent::HandsFreePress,
            &Mode::Auto,
            &DictationApp::Wispr,
            now,
        );
        gesture.handle(InputEvent::Release, &Mode::Auto, &DictationApp::Wispr, now);
        assert!(gesture.active && gesture.latched);
        gesture.handle(InputEvent::Press, &Mode::Auto, &DictationApp::Wispr, now);
        gesture.handle(
            InputEvent::HandsFreePress,
            &Mode::Auto,
            &DictationApp::Wispr,
            now,
        );
        gesture.handle(InputEvent::Release, &Mode::Auto, &DictationApp::Wispr, now);
        assert!(!gesture.active);
        assert!(gesture.second_deadline.is_none());
    }

    #[test]
    fn escape_cancels_and_a_late_release_cannot_restart() {
        let mut gesture = Gesture::default();
        let now = Instant::now();
        gesture.handle(
            InputEvent::HandsFreePress,
            &Mode::Auto,
            &DictationApp::Wispr,
            now,
        );
        gesture.handle(InputEvent::Dismiss, &Mode::Auto, &DictationApp::Wispr, now);
        gesture.handle(InputEvent::Release, &Mode::Auto, &DictationApp::Wispr, now);
        assert!(!gesture.active);
        assert!(gesture.second_deadline.is_none());
    }

    #[test]
    fn wispr_double_tap_window_is_half_a_second_from_start() {
        let mut gesture = Gesture::default();
        let now = Instant::now();
        gesture.handle(InputEvent::Press, &Mode::Auto, &DictationApp::Wispr, now);
        gesture.handle(
            InputEvent::Release,
            &Mode::Auto,
            &DictationApp::Wispr,
            now + Duration::from_millis(50),
        );
        assert_eq!(
            gesture.second_deadline,
            Some(now + Duration::from_millis(500))
        );
    }

    #[test]
    fn hold_and_toggle_modes_keep_their_original_behavior() {
        let now = Instant::now();
        let mut gesture = Gesture::default();
        gesture.handle(InputEvent::Press, &Mode::Hold, &DictationApp::Willow, now);
        gesture.handle(InputEvent::Release, &Mode::Hold, &DictationApp::Willow, now);
        assert!(!gesture.active);
        gesture.handle(InputEvent::Press, &Mode::Toggle, &DictationApp::Willow, now);
        gesture.handle(
            InputEvent::Release,
            &Mode::Toggle,
            &DictationApp::Willow,
            now,
        );
        assert!(gesture.active);
        gesture.handle(InputEvent::Press, &Mode::Toggle, &DictationApp::Willow, now);
        assert!(!gesture.active);
    }
}
