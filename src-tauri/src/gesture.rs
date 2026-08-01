use crate::{
    config::{ConfigStore, Mode},
    discord::{BridgeStatus, DiscordCommand},
    input::InputEvent,
};
use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

pub fn start_gesture_worker(
    rx: mpsc::Receiver<InputEvent>,
    discord: mpsc::Sender<DiscordCommand>,
    store: Arc<ConfigStore>,
    status: Arc<Mutex<BridgeStatus>>,
) {
    thread::Builder::new()
        .name("willow-gesture".into())
        .spawn(move || {
            let mut active = false;
            let mut latched = false;
            let mut stopping = false;
            let mut down_at: Option<Instant> = None;
            let mut second_deadline: Option<Instant> = None;

            loop {
                let received = match second_deadline {
                    Some(deadline) => {
                        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    }
                    None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                match received {
                    Ok(InputEvent::Press) => {
                        let config = store.get();
                        match config.mode {
                            Mode::Hold => activate(&mut active, &discord, &status),
                            Mode::Toggle => {
                                if active {
                                    deactivate(
                                        &mut active,
                                        &discord,
                                        &status,
                                        config.unmute_delay_ms,
                                    );
                                } else {
                                    activate(&mut active, &discord, &status);
                                }
                            }
                            Mode::Auto => {
                                let now = Instant::now();
                                if latched {
                                    stopping = true;
                                    down_at = Some(now);
                                } else if second_deadline.is_some_and(|deadline| now <= deadline) {
                                    second_deadline = None;
                                    latched = true;
                                    down_at = Some(now);
                                } else {
                                    second_deadline = None;
                                    down_at = Some(now);
                                    activate(&mut active, &discord, &status);
                                }
                            }
                        }
                    }
                    Ok(InputEvent::Release) => {
                        let config = store.get();
                        match config.mode {
                            Mode::Hold => {
                                deactivate(&mut active, &discord, &status, config.unmute_delay_ms)
                            }
                            Mode::Toggle => {}
                            Mode::Auto => {
                                if latched {
                                    if stopping {
                                        stopping = false;
                                        latched = false;
                                        deactivate(
                                            &mut active,
                                            &discord,
                                            &status,
                                            config.unmute_delay_ms,
                                        );
                                    }
                                } else if down_at.take().is_some_and(|start| {
                                    start.elapsed() > Duration::from_millis(250)
                                }) {
                                    deactivate(
                                        &mut active,
                                        &discord,
                                        &status,
                                        config.unmute_delay_ms,
                                    );
                                } else {
                                    second_deadline =
                                        Some(Instant::now() + Duration::from_millis(350));
                                }
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        second_deadline = None;
                        let delay = store.get().unmute_delay_ms;
                        deactivate(&mut active, &discord, &status, delay);
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        deactivate(&mut active, &discord, &status, 0);
                        break;
                    }
                }
            }
        })
        .expect("failed to start gesture worker");
}

fn activate(
    active: &mut bool,
    discord: &mpsc::Sender<DiscordCommand>,
    status: &Mutex<BridgeStatus>,
) {
    if *active {
        return;
    }
    *active = true;
    if let Ok(mut status) = status.lock() {
        status.active = true;
    }
    let _ = discord.send(DiscordCommand::SetMute(true));
}

fn deactivate(
    active: &mut bool,
    discord: &mpsc::Sender<DiscordCommand>,
    status: &Mutex<BridgeStatus>,
    delay_ms: u64,
) {
    if !*active {
        return;
    }
    *active = false;
    if let Ok(mut status) = status.lock() {
        status.active = false;
    }
    if delay_ms > 0 {
        thread::sleep(Duration::from_millis(delay_ms));
    }
    let _ = discord.send(DiscordCommand::SetMute(false));
}
