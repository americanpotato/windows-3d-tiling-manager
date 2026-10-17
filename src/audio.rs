//! Per-app muting through Core Audio sessions (the speaker button of window cards).
//!
//! While the 3D view is open, apps whose cards have the speaker off are muted. Windows keeps a
//! session's mute per app across restarts (there is no temporary mute), so every mute set here
//! must be undone: the apps muted are written to `muted.json` before they are muted, and every
//! session of those apps is unmuted when the view closes, when the app exits or panics, and at
//! the next start. An app that was closed while muted comes back muted, so the ledger is kept
//! (and checked every few seconds) until a session of it has been seen and unmuted. Mutes set
//! by the user elsewhere are left alone.

use std::collections::{HashMap, HashSet};

use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::Interface;

#[derive(Default)]
pub struct Muter {
    /// Processes whose sessions we muted.
    muted: HashSet<u32>,
    /// Lower-case image path per process id.
    paths: HashMap<u32, String>,
    /// Lower-case image paths of apps we muted and have not seen unmuted since (`muted.json`).
    ledger: HashSet<String>,
}


fn ledger_path() -> std::path::PathBuf {
    crate::store::app_dir().join("muted.json")
}

fn load_ledger() -> HashSet<String> {
    std::fs::read_to_string(ledger_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn save_ledger(ledger: &HashSet<String>) {
    let path = ledger_path();
    if ledger.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let mut v: Vec<&String> = ledger.iter().collect();
    v.sort();
    if let Ok(s) = serde_json::to_string_pretty(&v)
        && let Err(e) = std::fs::write(&path, s)
    {
        crate::store::log(&format!("failed to save muted.json: {e}"));
    }
}

/// Unmutes what an earlier run (or this one) muted, without the in-memory state: for the panic
/// hook and the end of the Windows session.
pub fn emergency_restore() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    Muter::load().restore();
}

impl Muter {
    /// With the ledger of earlier runs (a muter without it, as in the selftest, never touches
    /// mutes it did not set).
    pub fn load() -> Self {
        Self { ledger: load_ledger(), ..Default::default() }
    }

    /// Mutes every audio session (on every active output device) of a process whose image path
    /// is in `mute` (lower-case), and unmutes the ones muted earlier that are no longer in it.
    /// Sessions are matched by executable rather than by window process because browsers and
    /// other multi-process apps play their sound from a child process.
    pub fn apply(&mut self, mute: &HashSet<String>) {
        if mute.is_empty() && self.muted.is_empty() && self.ledger.is_empty() {
            return;
        }
        let before = self.ledger.clone();
        if let Err(e) = self.apply_inner(mute) {
            crate::store::log(&format!("audio mute failed: {e}"));
        }
        if self.ledger != before {
            save_ledger(&self.ledger);
        }
    }

    /// Gives back the sound of everything muted here (or by an earlier run).
    pub fn restore(&mut self) {
        self.apply(&HashSet::new());
    }

    /// Apps muted earlier whose sessions have not been seen since (they were closed while
    /// muted): `restore` needs calling again until this is false.
    pub fn pending(&self) -> bool {
        !self.ledger.is_empty()
    }

    fn apply_inner(&mut self, mute: &HashSet<String>) -> windows::core::Result<()> {
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let devices = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut still = HashSet::new();
            // Ledger apps whose sessions were seen and unmuted.
            let mut given_back = HashSet::new();
            for d in 0..devices.GetCount()? {
                let Ok(dev) = devices.Item(d) else { continue };
                let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                    continue;
                };
                let Ok(sessions) = mgr.GetSessionEnumerator() else {
                    continue;
                };
                for k in 0..sessions.GetCount().unwrap_or(0) {
                    let Ok(s) = sessions.GetSession(k) else {
                        continue;
                    };
                    let Ok(s2) = s.cast::<IAudioSessionControl2>() else {
                        continue;
                    };
                    let pid = s2.GetProcessId().unwrap_or(0);
                    if pid == 0 {
                        continue;
                    }
                    let path = self
                        .paths
                        .entry(pid)
                        .or_insert_with(|| crate::win::process_path(pid).to_lowercase());
                    let Ok(vol) = s.cast::<ISimpleAudioVolume>() else {
                        continue;
                    };
                    let is_muted = vol.GetMute().is_ok_and(|m| m.as_bool());
                    if mute.contains(path.as_str()) {
                        if !self.muted.contains(&pid) && is_muted && !self.ledger.contains(path.as_str()) {
                            // Already muted by someone else: not ours to unmute later.
                            continue;
                        }
                        if !is_muted {
                            // Recorded before muting, so a crash in between still leaves the
                            // app to unmute.
                            if self.ledger.insert(path.clone()) {
                                save_ledger(&self.ledger);
                            }
                            let _ = vol.SetMute(true, std::ptr::null());
                        }
                        still.insert(pid);
                    } else if self.muted.contains(&pid) || self.ledger.contains(path.as_str()) {
                        if is_muted {
                            let _ = vol.SetMute(false, std::ptr::null());
                        }
                        given_back.insert(path.clone());
                    }
                }
            }
            self.muted = still;
            for p in given_back {
                self.ledger.remove(&p);
            }
            // Forget paths of processes that may have exited (pids get reused).
            if self.paths.len() > 512 {
                self.paths.clear();
            }
        }
        Ok(())
    }
}
