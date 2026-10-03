use crate::discord::DiscordCommand;
use serde::Deserialize;
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime},
};
use windows::Win32::Media::{Audio::*, MM_MIM_DATA};

#[derive(Deserialize)]
struct Settings {
    device: String,
    channel: u8,
    pads: Vec<Pad>,
}

#[derive(Deserialize)]
struct Pad {
    control: u8,
    name: String,
    sound_id: String,
}

impl Settings {
    fn validate(&self) -> bool {
        !self.device.is_empty()
            && (1..=16).contains(&self.channel)
            && self.pads.iter().all(|pad| {
                pad.control < 128
                    && !pad.sound_id.is_empty()
                    && pad.sound_id.len() <= 20
                    && pad.sound_id.bytes().all(|c| c.is_ascii_digit())
            })
            && self.pads.iter().enumerate().all(|(i, p)| {
                !self.pads[..i]
                    .iter()
                    .any(|other| other.control == p.control)
            })
    }

    fn pad(&self, message: u32) -> Option<&Pad> {
        let status = (message & 255) as u8;
        let control = ((message >> 8) & 127) as u8;
        let value = (message >> 16) & 127;
        if status != 0xb0 | (self.channel - 1) || value == 0 {
            return None;
        }
        self.pads.iter().find(|pad| pad.control == control)
    }
}

unsafe extern "system" fn callback(
    _: HMIDIIN,
    message: u32,
    instance: usize,
    data: usize,
    _: usize,
) {
    if message == MM_MIM_DATA {
        // Keep all Discord I/O off WinMM's callback thread.
        let sender = unsafe { &*(instance as *const mpsc::SyncSender<u32>) };
        let _ = sender.try_send(data as u32);
    }
}

fn find_device(name: &str) -> Option<u32> {
    for id in 0..unsafe { midiInGetNumDevs() } {
        let mut caps = MIDIINCAPSW::default();
        if unsafe { midiInGetDevCapsW(id as usize, &mut caps, size_of::<MIDIINCAPSW>() as u32) }
            == 0
        {
            let device_name = caps.szPname;
            let end = device_name
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(device_name.len());
            if String::from_utf16_lossy(&device_name[..end]).eq_ignore_ascii_case(name) {
                return Some(id);
            }
        }
    }
    None
}

pub fn start(path: PathBuf, discord: mpsc::Sender<DiscordCommand>) {
    if !path.exists() {
        return;
    }
    thread::Builder::new()
        .name("midi-pads".into())
        .spawn(move || {
            let log_path = path.with_file_name("midi-pads.log");
            let log = |text: &str| {
                if fs::metadata(&log_path).is_ok_and(|m| m.len() > 1_000_000) {
                    let _ = fs::rename(&log_path, log_path.with_extension("previous.log"));
                }
                if let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                {
                    let _ = writeln!(file, "{:?} {text}", SystemTime::now());
                }
            };
            let settings = fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Settings>(&raw).ok());
            let Some(settings) = settings.filter(Settings::validate) else {
                log("Invalid MIDI pad settings");
                return;
            };
            let (sender, receiver) = mpsc::sync_channel::<u32>(32);
            let sender = Box::new(sender);
            loop {
                let Some(id) = find_device(&settings.device) else {
                    thread::sleep(Duration::from_secs(2));
                    continue;
                };
                let mut handle = HMIDIIN::default();
                let opened = unsafe {
                    midiInOpen(
                        &mut handle,
                        id,
                        Some(callback as *const () as usize),
                        Some((&*sender as *const mpsc::SyncSender<u32>) as usize),
                        CALLBACK_FUNCTION,
                    )
                };
                if opened != 0 {
                    log(&format!("MIDI open failed: {opened}"));
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
                let started = unsafe { midiInStart(handle) };
                if started != 0 {
                    unsafe {
                        midiInClose(handle);
                    }
                    log(&format!("MIDI start failed: {started}"));
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
                log(&format!(
                    "Listening on {} channel {}",
                    settings.device, settings.channel
                ));
                let mut last_press: Option<(u32, Instant)> = None;
                loop {
                    match receiver.recv_timeout(Duration::from_secs(2)) {
                        Ok(message) => {
                            let Some(pad) = settings.pad(message) else {
                                continue;
                            };
                            if last_press.is_some_and(|(prior, at)| {
                                prior == message && at.elapsed() < Duration::from_millis(150)
                            }) {
                                continue;
                            }
                            last_press = Some((message, Instant::now()));
                            let (reply, result) = mpsc::sync_channel(1);
                            if discord
                                .send(DiscordCommand::PlaySound(pad.sound_id.clone(), reply))
                                .is_err()
                            {
                                break;
                            }
                            match result.recv_timeout(Duration::from_secs(10)) {
                                Ok(Ok(true)) => {
                                    log(&format!("Played {} ({})", pad.name, pad.sound_id))
                                }
                                Ok(Ok(false)) => {
                                    log(&format!("Skipped {}: guild not allowed", pad.name))
                                }
                                Ok(Err(error)) => log(&format!("{}: {error}", pad.name)),
                                Err(_) => log("Discord playback timed out; not retried"),
                            }
                            // Drop accumulated button presses rather than play stale sounds.
                            while receiver.try_recv().is_ok() {}
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if find_device(&settings.device) != Some(id) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                unsafe {
                    midiInStop(handle);
                    midiInReset(handle);
                    midiInClose(handle);
                }
                log("MIDI disconnected; waiting for device");
            }
        })
        .expect("start MIDI pad thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> Settings {
        Settings {
            device: "MIDI function".into(),
            channel: 16,
            pads: vec![Pad {
                control: 60,
                name: "Test".into(),
                sound_id: "1552414182845055028".into(),
            }],
        }
    }
    #[test]
    fn only_matching_control_on_plays() {
        let config = settings();
        assert!(config.pad(0x7f3cbf).is_some());
        for message in [0x003cbf, 0x7f3c8f, 0x7f3cb0, 0x7f3dbf, 0x7f3c9f] {
            assert!(config.pad(message).is_none());
        }
    }
    #[test]
    fn invalid_settings_are_rejected() {
        let mut config = settings();
        assert!(config.validate());
        config.channel = 0;
        assert!(!config.validate());
        config.channel = 16;
        config.pads[0].sound_id = "invalid".into();
        assert!(!config.validate());
    }
}
