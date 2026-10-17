//! Numbers for the stats panel: process CPU and memory, and a few latencies measured where
//! they happen (switching to an app, passing input on).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// CPU and memory of one process at one moment.
pub struct ProcSample {
    pub pid: u32,
    cpu: u64,
    t: Instant,
    /// Share of the whole machine since the previous sample (100 = every core busy).
    pub cpu_pct: Option<f32>,
    pub working_set: u64,
    pub private: u64,
}

fn ft(f: FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

/// CPU time used by this process so far (kernel + user), in 100 ns units.
pub fn cpu_time() -> u64 {
    let (mut c, mut e, mut k, mut u) = Default::default();
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    ft(k) + ft(u)
}

fn read(h: HANDLE) -> Option<(u64, u64, u64)> {
    let (mut c, mut e, mut k, mut u) = Default::default();
    unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).ok()? };
    let mut m = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    unsafe {
        GetProcessMemoryInfo(
            h,
            &mut m as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            m.cb,
        )
        .ok()?
    };
    Some((
        ft(k) + ft(u),
        m.WorkingSetSize as u64,
        m.PrivateUsage as u64,
    ))
}

/// Samples process `pid` (0 = this one); `prev` (same pid) gives the CPU share.
pub fn sample(pid: u32, prev: Option<&ProcSample>) -> Option<ProcSample> {
    let (cpu, ws, private) = if pid == 0 {
        read(unsafe { GetCurrentProcess() })?
    } else {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? };
        let r = read(h);
        unsafe {
            let _ = CloseHandle(h);
        }
        r?
    };
    let t = Instant::now();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f32;
    let cpu_pct = prev.filter(|p| p.pid == pid).and_then(|p| {
        let wall = (t - p.t).as_secs_f32();
        (wall > 0.05).then(|| cpu.saturating_sub(p.cpu) as f32 / 1e7 / wall / cores * 100.0)
    });
    Some(ProcSample {
        pid,
        cpu,
        t,
        cpu_pct,
        working_set: ws,
        private,
    })
}

struct Latency {
    /// Window being switched to, and when that was asked for.
    pending: Option<(isize, Instant)>,
    /// Last measured switch time per window, in milliseconds.
    switch: Option<HashMap<isize, f32>>,
    /// Smoothed input passing delay, in milliseconds.
    input: Option<f32>,
}

static LATENCY: Mutex<Latency> = Mutex::new(Latency {
    pending: None,
    switch: None,
    input: None,
});

fn with<R>(f: impl FnOnce(&mut Latency) -> R) -> R {
    f(&mut LATENCY.lock().unwrap_or_else(|e| e.into_inner()))
}

/// We are about to give window `h` the foreground.
pub fn switch_start(h: isize) {
    with(|l| l.pending = Some((h, Instant::now())));
}

/// Window `h` became the foreground window.
pub fn switch_done(h: isize) {
    with(|l| {
        if let Some((p, t0)) = l.pending.take_if(|p| p.0 == h) {
            l.switch
                .get_or_insert_with(HashMap::new)
                .insert(p, t0.elapsed().as_secs_f32() * 1000.0);
        } else if l
            .pending
            .is_some_and(|p| p.1.elapsed() > Duration::from_secs(5))
        {
            l.pending = None;
        }
    })
}

pub fn switch_ms(h: isize) -> Option<f32> {
    with(|l| l.switch.as_ref()?.get(&h).copied())
}

/// A message took `d` from its arrival in our window procedure to being posted to the app.
pub fn record_input(d: Duration) {
    let ms = d.as_secs_f32() * 1000.0;
    with(|l| l.input = Some(l.input.map_or(ms, |o| o * 0.9 + ms * 0.1)));
}

pub fn input_ms() -> Option<f32> {
    with(|l| l.input)
}

/// What the open 3D view reports (twice a second) for the stats window.
#[derive(Clone, Copy, Debug)]
pub struct ViewStats {
    /// Smoothed frame time of the 3D view.
    pub frame_ms: f32,
    /// Live cards, their average fps and capture delay, and the worst delay.
    pub live: usize,
    pub fps: f32,
    pub delay_ms: f32,
    pub delay_max: f32,
    pub at: Instant,
}

static VIEW: Mutex<Option<ViewStats>> = Mutex::new(None);

pub fn set_view(v: ViewStats) {
    *VIEW.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
}

/// The 3D view's numbers, or `None` once it stopped reporting (it is closed).
pub fn view() -> Option<ViewStats> {
    let v = *VIEW.lock().unwrap_or_else(|e| e.into_inner());
    v.filter(|v| v.at.elapsed() < Duration::from_secs(2))
}
