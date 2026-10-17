//! Live window content through Windows.Graphics.Capture. Frames arrive on a D3D11 device, are
//! copied into a CPU-readable staging texture and written straight into a wgpu texture. Windows
//! only produce frames when their content changes, so idle live cards cost next to nothing.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_WAS_STILL_DRAWING, IDXGIDevice};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{Interface, Result};

use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_UNIT_MODE_PIXELS, D2D1CreateDevice,
    ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Image,
};
use windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET;
use windows::Win32::Graphics::Dxgi::IDXGISurface;

use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_USAGE_DEFAULT, ID3D11Device5,
    ID3D11DeviceContext4, ID3D11Fence,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_FENCE_FLAG_SHARED, ID3D12CommandQueue, ID3D12Device, ID3D12Fence, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM_SRGB;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter,
    IDXGIFactory4, IDXGIResource1,
};

use crate::render::{LiveTex, Renderer, Share};

thread_local! {
    /// Where the time of the poll going on went, for the log when it is slow.
    static PARTS: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Notes how long since `t0` the poll going on has taken, at the point `name`.
fn part(t0: Instant, name: &str) {
    use std::fmt::Write;
    PARTS.with(|p| {
        let _ = write!(p.borrow_mut(), " {name}={}", t0.elapsed().as_micros());
    });
}

/// The D3D11 device and wgpu's D3D12 queue, tied by two shared fences: `done` is signalled by
/// D3D11 when a copy into a shared texture is finished and waited on by the wgpu queue before it
/// draws; `free` is signalled by the wgpu queue after what it was given so far, and a shared
/// texture taken out of view is written again only once it is reached. (D3D11 waiting on it held
/// up the whole frame on the CPU, 15 to 50 ms, until the GPU had drawn the one before.)
struct Shared {
    wgpu: wgpu::Device,
    d3d12: ID3D12Device,
    queue: ID3D12CommandQueue,
    context: ID3D11DeviceContext4,
    done11: ID3D11Fence,
    done12: ID3D12Fence,
    done: u64,
    free12: ID3D12Fence,
    free: u64,
}

/// A texture made on D3D11 and opened in wgpu.
struct SharedTex(ID3D11Texture2D, wgpu::Texture);
unsafe impl Send for SharedTex {}

/// A D3D12 fence made shareable and opened on the D3D11 device.
fn shared_fence(d3d12: &ID3D12Device, d3d11: &ID3D11Device5) -> Result<(ID3D12Fence, ID3D11Fence)> {
    unsafe {
        let f12: ID3D12Fence = d3d12.CreateFence(0, D3D12_FENCE_FLAG_SHARED)?;
        let h = d3d12.CreateSharedHandle(&f12, None, GENERIC_ALL.0, None)?;
        let mut f11: Option<ID3D11Fence> = None;
        let r = d3d11.OpenSharedFence(h, &mut f11);
        let _ = CloseHandle(h);
        r?;
        Ok((f12, f11.ok_or_else(windows::core::Error::empty)?))
    }
}

/// Makes a `w`x`h` texture on D3D11 and opens it in wgpu (slow for a big one: run on a worker).
fn make_shared(
    device: &ID3D11Device,
    d3d12: &ID3D12Device,
    wgpu: &wgpu::Device,
    w: u32,
    h: u32,
) -> Option<SharedTex> {
    let max_dim = wgpu.limits().max_texture_dimension_2d;
    let (w, h) = (w.clamp(1, max_dim), h.clamp(1, max_dim));
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
    };
    unsafe {
        let mut tex = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex)).ok()?;
        let tex: ID3D11Texture2D = tex?;
        let handle = tex
            .cast::<IDXGIResource1>()
            .ok()?
            .CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            )
            .ok()?;
        let mut res: Option<ID3D12Resource> = None;
        let r = d3d12.OpenSharedHandle(handle, &mut res);
        let _ = CloseHandle(handle);
        r.ok()?;
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let hal = wgpu::hal::dx12::Device::texture_from_raw(
            res?,
            format,
            wgpu::TextureDimension::D2,
            size,
            1,
            1,
        );
        let wt = wgpu.create_texture_from_hal::<wgpu::hal::api::Dx12>(
            hal,
            &wgpu::TextureDescriptor {
                label: Some("live shared"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::TextureUses::RESOURCE,
        );
        Some(SharedTex(tex, wt))
    }
}

impl Shared {
    /// Makes a D3D11 device on wgpu's adapter, with the fences to share textures with wgpu.
    fn new(share: Share) -> Result<(Self, ID3D11Device, ID3D11DeviceContext)> {
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
            let adapter: IDXGIAdapter = factory.EnumAdapterByLuid(share.luid)?;
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device: ID3D11Device = device.ok_or_else(windows::core::Error::empty)?;
            let context: ID3D11DeviceContext = context.ok_or_else(windows::core::Error::empty)?;
            let d5: ID3D11Device5 = device.cast()?;
            let (done12, done11) = shared_fence(&share.d3d12, &d5)?;
            let (free12, _) = shared_fence(&share.d3d12, &d5)?;
            let shared = Self {
                wgpu: share.device,
                d3d12: share.d3d12,
                queue: share.queue,
                context: context.cast()?,
                done11,
                done12,
                done: 0,
                free12,
                free: 0,
            };
            Ok((shared, device, context))
        }
    }

    /// Copies the top-left `w`x`h` of `src` into `dst` (out of view: see [`Live::swap`]), and
    /// has wgpu wait for the copy before drawing anything more.
    fn copy(&mut self, dst: &ID3D11Texture2D, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<()> {
        unsafe {
            let b = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            self.context
                .CopySubresourceRegion(dst, 0, 0, 0, 0, src, 0, Some(&b));
            self.done += 1;
            self.context.Signal(&self.done11, self.done)?;
            self.context.Flush();
            self.queue.Wait(&self.done12, self.done)?;
        }
        Ok(())
    }

    /// Marks what wgpu was given so far: the value of `free` it is done with at.
    fn release(&mut self) -> Result<u64> {
        self.free += 1;
        unsafe { self.queue.Signal(&self.free12, self.free)? };
        Ok(self.free)
    }

    /// Whether wgpu is done with what it was given up to `free` value `v`.
    fn reached(&self, v: u64) -> bool {
        unsafe { self.free12.GetCompletedValue() >= v }
    }
}

/// One Windows.Graphics.Capture session and the staging texture its frames are read through.
struct Capture {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
    /// Content size of the last frame taken.
    content: SizeInt32,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// A frame copied into `staging` but not read yet (its size and composition time): reading
    /// it back right after the copy would wait for the GPU, so it is read on the next poll.
    copied: Option<(u32, u32, Option<i64>)>,
    /// A staging texture being made on a worker for a new frame size (that takes tens of
    /// milliseconds for a big window), and the frame waiting for it.
    making: Option<(u32, u32, Receiver<Option<Agile<ID3D11Texture2D>>>)>,
    waiting: Option<Direct3D11CaptureFrame>,
    /// The window being captured, to reopen the capture.
    hwnd: isize,
    /// When the pool was last recreated for a new window size, until a frame of that size
    /// arrives. A window that doesn't redraw sends no frame after a resize (one restored from
    /// minimized would stay at its minimized size), so the capture is reopened if none comes.
    resized: Option<Instant>,
    /// The least time between frames asked of Windows (100 ns units; -1: not asked yet).
    min_gap: i64,
}

impl Capture {
    /// Asks Windows to make frames at least `gap` (100 ns units) apart: each frame of a big
    /// window is work for DWM even if it is never taken. Needs Windows 11 24H2; else ignored.
    fn set_min_gap(&mut self, gap: i64) {
        if self.min_gap != gap {
            self.min_gap = gap;
            let _ = self
                .session
                .SetMinUpdateInterval(windows::Foundation::TimeSpan { Duration: gap });
        }
    }
}

/// A D3D11 object handed between threads: D3D11 devices and resources are free-threaded.
struct Agile<T>(T);
unsafe impl<T> Send for Agile<T> {}

impl Drop for Capture {
    fn drop(&mut self) {
        // Closing a session can take tens of milliseconds: do it off the UI thread.
        let (session, pool) = (self.session.clone(), self.pool.clone());
        std::thread::spawn(move || {
            let _ = session.Close();
            let _ = pool.Close();
        });
    }
}

/// The desktop card's extra parts: Progman spans every monitor and has no taskbar, so its frames
/// are cropped to the primary monitor and the taskbar's own capture is drawn over them.
struct Desktop {
    progman: isize,
    tray: Option<Capture>,
    /// The latest taskbar frame, tightly packed.
    tray_px: Vec<u8>,
    tray_size: (u32, u32),
    /// The cropped desktop alone, so a new taskbar frame can be drawn over it again.
    desk: Vec<u8>,
    /// `desk` with the taskbar over it, tightly packed.
    out: Vec<u8>,
    out_size: (u32, u32),
    /// With GPU sharing: the latest taskbar frame (its texture and the size of its content).
    tray_tex: Option<(ID3D11Texture2D, u32, u32)>,
    /// The shell's popups open on the primary monitor (Start, search, the taskbar's previews and
    /// menus), topmost first, each captured on its own and drawn over the rest.
    pops: Vec<Pop>,
    pop_scan: Option<Instant>,
    pop_tx: Sender<(isize, Result<Capture>)>,
    pop_rx: Receiver<(isize, Result<Capture>)>,
}

struct Pop {
    hwnd: isize,
    /// Top left, relative to the primary monitor (the desktop's) or to the window's frame.
    at: POINT,
    cap: Option<Capture>,
    px: Vec<u8>,
    size: (u32, u32),
    /// With GPU sharing (popups of a window): the latest frame, to draw with Direct2D.
    tex: Option<PopTex>,
}

/// A popup's latest frame on the GPU, at the size of its capture.
struct PopTex {
    tex: ID3D11Texture2D,
    bmp: ID2D1Bitmap1,
    w: u32,
    h: u32,
}

/// The popups of the window in use: menus, drop-downs, tooltips and dialogs are windows of
/// their own, left out of its capture, so each is captured on its own and drawn over its frames
/// where it is (the real ones are made invisible on the screen: `win::popups_of`).
struct Popups {
    /// Topmost first.
    list: Vec<Pop>,
    scan: Option<Instant>,
    tx: Sender<(isize, Result<Capture>)>,
    rx: Receiver<(isize, Result<Capture>)>,
    /// With GPU sharing: the window's latest frame alone and with the popups drawn over it,
    /// made on a worker when popups first show.
    gpu: Option<Compose>,
    making: Option<(u32, u32, Receiver<Option<Agile<(ID3D11Texture2D, ID3D11Texture2D)>>>)>,
    failed: bool,
    /// Without: the window's latest frame, tightly packed (`fresh` while it is the one shown),
    /// and with the popups drawn over it.
    px: Vec<u8>,
    fresh: bool,
    out: Vec<u8>,
    size: (u32, u32),
}

struct Compose {
    base: ID3D11Texture2D,
    comp: ID3D11Texture2D,
    target: ID2D1Bitmap1,
    size: (u32, u32),
    /// Just made: `base` is filled from the frame shown before the popups are drawn.
    fresh: bool,
}

struct Stream {
    cap: Capture,
    pops: Popups,
    /// Not polled again before this, unless it is the card in use.
    due: Instant,
    desktop: Option<Desktop>,
    /// wgpu texture slot holding the latest frame, once one arrived.
    slot: Option<usize>,
    slot_size: (u32, u32),
    /// How long the last upload to `slot` took (creating it, if it was new, and writing).
    upload_ms: f32,
    /// A texture being made for a new frame size, and that frame, tightly packed.
    making: Option<(u32, u32, Receiver<wgpu::Texture>)>,
    held: Vec<u8>,
    /// With GPU sharing: the D3D11 and wgpu sides of the texture in `slot`; the other of the
    /// pair, which new frames are copied into before it is swapped in, with the `free` value
    /// wgpu is done drawing from it at; and a pair being made for a new frame size.
    shared_tex: Option<ID3D11Texture2D>,
    shared_gpu: Option<LiveTex>,
    shared_back: Option<(ID3D11Texture2D, LiveTex, u64)>,
    shared_making: Option<(u32, u32, Receiver<Option<(SharedTex, SharedTex)>>)>,
    /// Smoothed capture-to-upload delay and copy time, in milliseconds.
    delay_ms: f32,
    copy_ms: f32,
    /// Frames counted since `fps_t0`, and the last full second's rate.
    count: u32,
    fps_t0: Instant,
    fps: f32,
    /// Frames taken in all (`count` restarts every second).
    frames: u64,
}

/// The WinRT device made from a DXGI device is free-threaded (the capture docs use it from any
/// thread), but windows-rs doesn't mark the interface as `Send`.
struct AgileDevice(IDirect3DDevice);
unsafe impl Send for AgileDevice {}

/// A capture session opened on a worker thread.
struct Opened {
    cap: Capture,
    /// For the desktop: the taskbar's capture (None if it could not be opened).
    tray: Option<Option<Capture>>,
    hwnd: isize,
}

/// Opens and starts a capture of `hwnd` (slow: up to ~150 ms per window).
/// The desktop's card is a capture of the whole primary monitor (the 3D view is left out of
/// screen captures), so it shows the real screen with its windows, taskbar and popups.
fn open(device: &IDirect3DDevice, hwnd: isize) -> Result<Opened> {
    let cap = open_capture(device, hwnd)?;
    Ok(Opened { cap, tray: None, hwnd })
}

fn open_capture(device: &IDirect3DDevice, hwnd: isize) -> Result<Capture> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe {
        if crate::win::is_desktop(crate::win::hwnd_of(hwnd)) {
            use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
            interop.CreateForMonitor(MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY))?
        } else {
            interop.CreateForWindow(HWND(hwnd as *mut core::ffi::c_void))?
        }
    };
    let size = item.Size()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    let _ = session.SetIsBorderRequired(false);
    session.StartCapture()?;
    Ok(Capture {
        _item: item,
        pool,
        session,
        size,
        content: size,
        staging: None,
        copied: None,
        making: None,
        waiting: None,
        hwnd,
        resized: None,
        min_gap: -1,
    })
}

/// The texture slot holding the latest frame of a stream.
pub struct Frame {
    pub slot: usize,
    /// Size of the window content in it.
    pub w: u32,
    pub h: u32,
}

/// Timing of a stream, for the stats panel.
pub struct LiveStats {
    /// From the moment Windows composed the frame to the end of our upload.
    pub delay_ms: f32,
    /// GPU copy + CPU map/upload time.
    pub copy_ms: f32,
    pub fps: f32,
}

/// QueryPerformanceCounter time in 100 ns units (the clock of `SystemRelativeTime`).
fn qpc_100ns() -> i64 {
    let (mut c, mut f) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    (c as i128 * 10_000_000 / f.max(1) as i128) as i64
}

/// Most frames per second taken from a live card that is off screen (but near enough to keep
/// its capture going) and not in use (0: every frame).
pub static LIVE_FPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(20);
/// Most frames per second the view draws (0: no limit): no use taking more of a card on screen
/// or in use.
pub static MAX_FPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(60);

pub struct Live {
    /// The card in use (in place or in theater mode): its frames are always taken.
    pub priority: Option<u64>,
    /// Cards on screen: their frames are taken as often as the view draws, like the one in use.
    pub visible: HashSet<u64>,
    streams: HashMap<u64, Stream>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    /// Captures being opened on worker threads, and their results.
    pending: HashSet<u64>,
    tx: Sender<(u64, Result<Opened>)>,
    rx: Receiver<(u64, Result<Opened>)>,
    failed: Vec<(u64, String)>,
    /// Sharing textures with wgpu, unless that could not be set up or failed.
    shared: Option<Shared>,
    /// Counts new frames taken from any stream, so the view knows when to redraw.
    pub fresh: u64,
    /// Draws the popups of the window in use over its frames (with GPU sharing).
    d2d: Option<ID2D1DeviceContext>,
    /// The card whose window's popups are hidden from the screen.
    popped: Option<u64>,
}

/// A `Live` moved from the worker that made it: the D3D11 device is free-threaded and the
/// immediate context is only ever used by one thread at a time.
pub struct Sendable(pub Live);
unsafe impl Send for Sendable {}

impl Live {
    /// With `share`, frames are handed to wgpu on the GPU if that can be set up.
    pub fn new(share: Option<Share>) -> Result<Self> {
        match share.map(Shared::new) {
            Some(Ok((shared, device, context))) => {
                return Self::with(device, context, Some(shared));
            }
            Some(Err(e)) => crate::store::log(&format!("live: no GPU sharing: {e}")),
            None => {}
        }
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device: ID3D11Device = device.ok_or_else(windows::core::Error::empty)?;
        let context = context.ok_or_else(windows::core::Error::empty)?;
        Self::with(device, context, None)
    }

    fn with(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        shared: Option<Shared>,
    ) -> Result<Self> {
        let dxgi: IDXGIDevice = device.cast()?;
        let winrt_device = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let (tx, rx) = channel();
        let d2d = shared.as_ref().and_then(|_| make_d2d(&device));
        if shared.is_some() && d2d.is_none() {
            crate::store::log("live: no Direct2D: popups of a window in use are not drawn on its card");
        }
        Ok(Self {
            d2d,
            popped: None,
            streams: HashMap::new(),
            device,
            context,
            winrt_device,
            pending: HashSet::new(),
            tx,
            rx,
            failed: Vec::new(),
            shared,
            fresh: 0,
            priority: None,
            visible: HashSet::new(),
        })
    }

    /// Whether frames are handed to wgpu on the GPU.
    pub fn is_shared(&self) -> bool {
        self.shared.is_some()
    }

    /// Whether card `id` is streaming or being started.
    pub fn is_live(&self, id: u64) -> bool {
        self.streams.contains_key(&id) || self.pending.contains(&id)
    }

    /// Starts streaming window `hwnd` as card `id`. The capture is opened on a worker thread
    /// (it would stall the first frame for ~150 ms per window); failures show up in `failed`.
    pub fn start(&mut self, id: u64, hwnd: isize) {
        if self.is_live(id) {
            return;
        }
        self.open_async(id, hwnd);
    }

    /// Opens a capture of `hwnd` for card `id` on a worker; `collect` takes it in.
    fn open_async(&mut self, id: u64, hwnd: isize) {
        self.pending.insert(id);
        let (device, tx) = (AgileDevice(self.winrt_device.clone()), self.tx.clone());
        std::thread::spawn(move || {
            let device = device;
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
            }
            let _ = tx.send((id, open(&device.0, hwnd)));
        });
    }

    /// Captures that could not be started since the last call, with the reason.
    pub fn failed(&mut self) -> Vec<(u64, String)> {
        std::mem::take(&mut self.failed)
    }

    /// Takes in the captures the workers opened.
    fn collect(&mut self) {
        while let Ok((id, r)) = self.rx.try_recv() {
            if !self.pending.remove(&id) {
                // Stopped meanwhile: dropping the session closes it.
                continue;
            }
            match r {
                // A reopened capture: the stream keeps its texture slot.
                Ok(o) if self.streams.contains_key(&id) => {
                    if let Some(s) = self.streams.get_mut(&id) {
                        s.cap = o.cap;
                    }
                }
                Ok(o) => {
                    let (pop_tx, pop_rx) = channel();
                    self.streams.insert(
                        id,
                        Stream {
                            cap: o.cap,
                            pops: Popups {
                                list: Vec::new(),
                                scan: None,
                                tx: pop_tx,
                                rx: pop_rx,
                                gpu: None,
                                making: None,
                                failed: false,
                                px: Vec::new(),
                                fresh: false,
                                out: Vec::new(),
                                size: (0, 0),
                            },
                            desktop: o.tray.map(|tray| {
                                let (pop_tx, pop_rx) = channel();
                                Desktop {
                                    progman: o.hwnd,
                                    tray,
                                    tray_px: Vec::new(),
                                    tray_size: (0, 0),
                                    desk: Vec::new(),
                                    out: Vec::new(),
                                    out_size: (0, 0),
                                    tray_tex: None,
                                    pops: Vec::new(),
                                    pop_scan: None,
                                    pop_tx,
                                    pop_rx,
                                }
                            }),
                            slot: None,
                            slot_size: (0, 0),
                            shared_tex: None,
                            shared_gpu: None,
                            shared_back: None,
                            shared_making: None,
                            upload_ms: 0.0,
                            making: None,
                            held: Vec::new(),
                            delay_ms: 0.0,
                            copy_ms: 0.0,
                            count: 0,
                            due: Instant::now(),
                            fps_t0: Instant::now(),
                            frames: 0,
                            fps: 0.0,
                        },
                    );
                }
                Err(e) => self.failed.push((id, e.to_string())),
            }
        }
    }

    /// Whether card `id` is streaming and has shown a frame.
    pub fn has_frame(&self, id: u64) -> bool {
        self.streams.get(&id).is_some_and(|s| s.slot.is_some())
    }

    /// Stops streaming card `id`, shrinking its texture slot to nothing.
    pub fn stop(&mut self, id: u64, renderer: &mut Renderer) {
        self.pending.remove(&id);
        if let Some(slot) = self.streams.remove(&id).and_then(|s| s.slot) {
            renderer.live_slot(Some(slot), 1, 1);
        }
    }

    /// Stops every stream (their texture slots must not be used afterwards).
    pub fn stop_all(&mut self) {
        self.pending.clear();
        self.streams.clear();
    }

    pub fn stats(&self, id: u64) -> Option<LiveStats> {
        let s = self.streams.get(&id)?;
        // A stream that went quiet shows 0 fps rather than its last busy second.
        let fps = if s.fps_t0.elapsed().as_secs_f32() > 2.0 {
            0.0
        } else {
            s.fps
        };
        Some(LiveStats {
            delay_ms: s.delay_ms,
            copy_ms: s.copy_ms,
            fps,
        })
    }

    /// The pixel at (`x`, `y`) of card `id`'s latest frame as BGRA, read back from the GPU
    /// (only with GPU sharing), for the self-test.
    pub fn selftest_pixel(&self, id: u64, x: u32, y: u32) -> Option<[u8; 4]> {
        let tex = self.streams.get(&id)?.shared_tex.as_ref()?;
        let st = make_staging(&self.device, 1, 1)?;
        let b = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + 1,
            bottom: y + 1,
            back: 1,
        };
        unsafe {
            self.context.CopySubresourceRegion(&st, 0, 0, 0, 0, tex, 0, Some(&b));
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
            let px = std::slice::from_raw_parts(m.pData as *const u8, 4);
            let out = [px[0], px[1], px[2], px[3]];
            self.context.Unmap(&st, 0);
            Some(out)
        }
    }

    /// Selftest: the whole frame of stream `id` as it is in its texture.
    pub fn selftest_frame(&self, id: u64) -> Option<crate::image::Image> {
        let tex = self.streams.get(&id)?.shared_tex.as_ref()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let (w, h) = (desc.Width, desc.Height);
        let st = make_staging(&self.device, w, h)?;
        unsafe {
            self.context.CopySubresourceRegion(&st, 0, 0, 0, 0, tex, 0, Some(&full(w, h)));
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
            let mut img = crate::image::Image::new(w, h);
            for y in 0..h as usize {
                let row = (m.pData as *const u8).add(y * m.RowPitch as usize);
                let src = std::slice::from_raw_parts(row, w as usize * 4);
                img.px[y * w as usize * 4..][..w as usize * 4].copy_from_slice(src);
            }
            self.context.Unmap(&st, 0);
            Some(img)
        }
    }

    pub fn ids(&self) -> Vec<u64> {
        self.streams.keys().chain(&self.pending).copied().collect()
    }

    /// Takes the newest frame of every stream into its texture slot. Returns the frames to draw.
    pub fn poll(&mut self, renderer: &mut Renderer) -> HashMap<u64, Frame> {
        self.collect();
        let mut out = HashMap::new();
        let mut dead = Vec::new();
        let fps = LIVE_FPS.load(std::sync::atomic::Ordering::Relaxed);
        let max_fps = MAX_FPS.load(std::sync::atomic::Ordering::Relaxed);
        let gap = std::time::Duration::from_secs_f32(1.0 / fps.max(1) as f32);
        for (&id, s) in self.streams.iter_mut() {
            let t0 = Instant::now();
            PARTS.with(|p| p.borrow_mut().clear());
            // Windows makes frames a little faster than they are looked at (so none is a whole
            // gap late): `MAX_FPS` for cards on screen or in use, `LIVE_FPS` for the others.
            let fast = self.priority == Some(id) || self.visible.contains(&id);
            let n = if fast { max_fps } else { fps };
            s.cap.set_min_gap(if n > 0 { 9_000_000 / n as i64 } else { 0 });
            // Cards off screen are only looked at `LIVE_FPS` times a second: Windows keeps
            // just the newest frame, so this skips the copies and redraws in between.
            if fps > 0 && !fast && t0 < s.due && s.slot.is_some() {
                let (w, h) = s.slot_size;
                if let Some(slot) = s.slot {
                    out.insert(id, Frame { slot, w, h });
                }
                continue;
            }
            // Never more than one gap ahead: the card in use is polled every frame, and its
            // due time racing ahead froze it for seconds once it was no longer in use.
            s.due = (s.due + gap).clamp(t0, t0 + gap);
            s.upload_ms = 0.0;
            let before = s.frames;
            // The window in use: its popups are drawn on its card.
            let top = (self.priority == Some(id) && s.desktop.is_none()).then_some(s.cap.hwnd);
            let r = Self::poll_one(
                &self.device,
                &self.context,
                &self.winrt_device,
                &mut self.shared,
                self.d2d.as_ref(),
                top,
                s,
                renderer,
            );
            match r {
                Ok(()) => {}
                Err(_) => dead.push(id),
            }
            if s.frames != before {
                self.fresh += 1;
            }
            let ms = t0.elapsed().as_millis();
            if ms >= 20 {
                crate::store::log(&format!(
                    "slow live poll {ms} ms: card {id} {:?} upload {:.0} desktop {}; us:{}",
                    s.slot_size,
                    s.upload_ms,
                    s.desktop.as_ref().map_or(0, |d| d.pops.len() + s.pops.list.len() + 1),
                    PARTS.with(|p| p.borrow().clone())
                ));
            }
            if let Some(slot) = s.slot {
                let (w, h) = s.slot_size;
                out.insert(id, Frame { slot, w, h });
            }
        }
        for id in dead {
            self.stop(id, renderer);
        }
        // No longer in use: its popups show on the screen again (open ones once they close).
        let using = self
            .priority
            .filter(|id| self.streams.get(id).is_some_and(|s| s.desktop.is_none()));
        if self.popped != using {
            if self.popped.is_some() {
                crate::win::give_back_popups(false);
            }
            self.popped = using;
        }
        // A resized window that sent no frame at its new size: a fresh capture always sends one.
        let stuck: Vec<(u64, isize)> = self
            .streams
            .iter()
            .filter(|(id, s)| {
                s.desktop.is_none()
                    && !self.pending.contains(id)
                    && s.cap.resized.is_some_and(|t| t.elapsed().as_secs_f32() > 0.25)
            })
            .map(|(&id, s)| (id, s.cap.hwnd))
            .collect();
        for (id, hwnd) in stuck {
            if let Some(s) = self.streams.get_mut(&id) {
                s.cap.resized = None;
            }
            self.open_async(id, hwnd);
        }
        out
    }

    fn poll_one(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        winrt_device: &IDirect3DDevice,
        shared: &mut Option<Shared>,
        d2d: Option<&ID2D1DeviceContext>,
        top: Option<isize>,
        s: &mut Stream,
        renderer: &mut Renderer,
    ) -> Result<()> {
        let t0 = Instant::now();
        let composed = match (shared.is_some(), d2d) {
            (true, _) if s.desktop.is_none() => Self::poll_shared(
                device,
                context,
                winrt_device,
                shared,
                d2d,
                top,
                s,
                renderer,
            )?,
            (true, Some(d2d)) if !s.pops.failed => {
                Self::poll_desktop_gpu(device, context, winrt_device, shared, d2d, s, renderer)?
            }
            _ => Self::poll_cpu(device, context, winrt_device, top, s, renderer)?,
        };
        let Some(composed) = composed else {
            return Ok(());
        };
        let ema = |old: f32, new: f32| {
            if old == 0.0 {
                new
            } else {
                old * 0.8 + new * 0.2
            }
        };
        s.copy_ms = ema(s.copy_ms, t0.elapsed().as_secs_f32() * 1000.0);
        if let Some(c) = composed {
            let d = (qpc_100ns() - c) as f32 / 10_000.0;
            if (0.0..5000.0).contains(&d) {
                s.delay_ms = ema(s.delay_ms, d);
            }
        }
        s.count += 1;
        s.frames += 1;
        let el = s.fps_t0.elapsed().as_secs_f32();
        if el >= 1.0 {
            s.fps = s.count as f32 / el;
            (s.count, s.fps_t0) = (0, Instant::now());
        }
        Ok(())
    }

    /// Copies the newest frame into the stream's shared texture on the GPU. Returns Some(the
    /// time Windows composed it) for a new frame. If sharing fails it is turned off for good and
    /// frames go through the CPU from then on.
    #[allow(clippy::too_many_arguments)]
    fn poll_shared(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        winrt_device: &IDirect3DDevice,
        shared: &mut Option<Shared>,
        d2d: Option<&ID2D1DeviceContext>,
        top: Option<isize>,
        s: &mut Stream,
        renderer: &mut Renderer,
    ) -> Result<Option<Option<i64>>> {
        if shared.is_none() || !Self::back_free(shared, s) {
            return Ok(None);
        }
        let t0 = Instant::now();
        let mut pops = s.pops.scan(top.filter(|_| d2d.is_some()), winrt_device);
        part(t0, "scan");
        if let Some(d2d) = d2d {
            pops |= s.pops.grab_gpu(device, context, d2d, winrt_device);
            pops |= s.pops.made(d2d);
        }
        part(t0, "grab");
        let next = s.cap.next_frame()?;
        part(t0, "frame");
        let Some((frame, tex, w, h, composed)) = next else {
            // Only the popups changed: drawn again over the window's last frame.
            if !pops {
                return Ok(None);
            }
            let back = s.shared_back.as_ref().map(|b| b.0.clone());
            let (Some(sh), Some(dst), Some(back), Some(d2d)) = (shared.as_mut(), &s.shared_tex, back, d2d)
            else {
                return Ok(None);
            };
            let p = &mut s.pops;
            let (w, h) = s.slot_size;
            let Some(c) = p.gpu.as_mut().filter(|c| c.size == (w, h)) else {
                if !p.list.is_empty() {
                    p.make(device, w, h);
                }
                return Ok(None);
            };
            if c.fresh {
                c.fresh = false;
                unsafe { context.CopySubresourceRegion(&c.base, 0, 0, 0, 0, dst, 0, Some(&full(w, h))) };
            }
            let r = p.draw(context, d2d).and_then(|()| {
                let comp = p.gpu.as_ref().map(|c| c.comp.clone());
                comp.map_or(Ok(()), |comp| sh.copy(&back, &comp, w, h))
            });
            if p.list.is_empty() {
                p.gpu = None;
            }
            if let Err(e) = r {
                crate::store::log(&format!("live: popups not drawn ({e})"));
                p.gpu = None;
                p.failed = true;
                return Ok(None);
            }
            Self::swap(sh, s, renderer)?;
            return Ok(Some(None));
        };
        if Self::shared_sized(device, shared, s, renderer, w, h) != Some(true) {
            s.cap.waiting = Some(frame);
            return Ok(None);
        }
        let (Some(sh), Some(dst)) = (shared.as_mut(), s.shared_back.as_ref().map(|b| b.0.clone())) else {
            return Ok(None);
        };
        let dst = &dst;
        // With popups showing, the frame goes in under them.
        let p = &mut s.pops;
        let r = match (p.gpu.as_mut().filter(|c| c.size == (w, h)), d2d) {
            (Some(c), Some(d2d)) if !p.list.is_empty() => {
                c.fresh = false;
                unsafe { context.CopySubresourceRegion(&c.base, 0, 0, 0, 0, &tex, 0, Some(&full(w, h))) };
                let comp = c.comp.clone();
                match p.draw(context, d2d) {
                    Ok(()) => sh.copy(dst, &comp, w, h),
                    Err(e) => {
                        crate::store::log(&format!("live: popups not drawn ({e})"));
                        (p.gpu, p.failed) = (None, true);
                        sh.copy(dst, &tex, w, h)
                    }
                }
            }
            _ => {
                if p.list.is_empty() {
                    p.gpu = None;
                } else {
                    p.make(device, w, h);
                }
                sh.copy(dst, &tex, w, h)
            }
        };
        part(t0, "copy");
        if let Err(e) = r {
            crate::store::log(&format!("live: shared copy failed ({e}), frames go through the CPU"));
            *shared = None;
            (s.shared_tex, s.shared_back) = (None, None);
            s.slot_size = (0, 0);
            return Ok(None);
        }
        Self::swap(sh, s, renderer)?;
        drop(frame);
        s.cap.follow_size(winrt_device)?;
        Ok(Some(composed))
    }

    /// Whether the stream's out-of-view shared texture can be written: wgpu is done with it.
    fn back_free(shared: &Option<Shared>, s: &Stream) -> bool {
        match (shared, &s.shared_back) {
            (Some(sh), Some((_, _, v))) => sh.reached(*v),
            _ => true,
        }
    }

    /// Shows the frame just copied into the stream's out-of-view shared texture, taking the
    /// one shown so far out of view.
    fn swap(sh: &mut Shared, s: &mut Stream, renderer: &mut Renderer) -> Result<()> {
        let (Some((bt, bg, _)), Some(t), Some(g)) =
            (s.shared_back.take(), s.shared_tex.take(), s.shared_gpu.take())
        else {
            return Ok(());
        };
        s.slot = Some(renderer.live_slot_set(s.slot, &bg));
        (s.shared_tex, s.shared_gpu) = (Some(bt), Some(bg));
        s.shared_back = Some((t, g, sh.release()?));
        Ok(())
    }

    /// Has the stream's shared texture be `w`x`h`, making a new one on a worker. Returns
    /// Some(true) once it is and Some(false) while it is being made; None if it could not be
    /// made: sharing is then turned off for good and frames go through the CPU.
    fn shared_sized(
        device: &ID3D11Device,
        shared: &mut Option<Shared>,
        s: &mut Stream,
        renderer: &mut Renderer,
        w: u32,
        h: u32,
    ) -> Option<bool> {
        let sh = shared.as_ref()?;
        if s.shared_tex.is_some() && s.slot.is_some() && s.slot_size == (w, h) {
            return Some(true);
        }
        if !matches!(&s.shared_making, Some((mw, mh, _)) if (*mw, *mh) == (w, h)) {
            let (tx, rx) = channel();
            let made = Agile((device.clone(), sh.d3d12.clone(), sh.wgpu.clone()));
            std::thread::spawn(move || {
                let made = made;
                let (device, d3d12, wgpu) = &made.0;
                let make = || make_shared(device, d3d12, wgpu, w, h);
                let _ = tx.send(make().zip(make()));
            });
            s.shared_making = Some((w, h, rx));
        }
        match s.shared_making.as_ref().map(|m| m.2.try_recv()) {
            Some(Ok(Some((SharedTex(t, wt), SharedTex(bt, bwt))))) => {
                let front = renderer.live_wrap(wt);
                s.slot = Some(renderer.live_slot_set(s.slot, &front));
                s.slot_size = (w, h);
                (s.shared_tex, s.shared_gpu) = (Some(t), Some(front));
                s.shared_back = Some((bt, renderer.live_wrap(bwt), 0));
                s.shared_making = None;
                Some(true)
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => Some(false),
            _ => {
                crate::store::log("live: shared texture failed, frames go through the CPU");
                *shared = None;
                s.shared_making = None;
                (s.shared_tex, s.shared_back) = (None, None);
                s.slot_size = (0, 0);
                None
            }
        }
    }

    /// The desktop with GPU sharing: the primary monitor's part of Progman's frame, the
    /// taskbar's frame and the shell popups' are put together on the GPU (through the CPU that
    /// took over 20 ms a frame at 2880x1800, slowing everything shown on it). Returns Some(the
    /// time Windows composed it) for a new picture. If drawing fails, the desktop goes through
    /// the CPU from then on.
    fn poll_desktop_gpu(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        winrt_device: &IDirect3DDevice,
        shared: &mut Option<Shared>,
        d2d: &ID2D1DeviceContext,
        s: &mut Stream,
        renderer: &mut Renderer,
    ) -> Result<Option<Option<i64>>> {
        let t0 = Instant::now();
        let Some(l) = s.desktop.as_ref().and_then(|d| crate::win::desktop_layout(d.progman)) else {
            return Ok(None);
        };
        part(t0, "layout");
        let c = l.crop;
        let size = ((c.right - c.left).max(1) as u32, (c.bottom - c.top).max(1) as u32);
        if Self::shared_sized(device, shared, s, renderer, size.0, size.1) != Some(true)
            || !Self::back_free(shared, s)
        {
            return Ok(None);
        }
        let Stream {
            cap,
            pops: p,
            desktop: Some(d),
            ..
        } = s
        else {
            return Ok(None);
        };
        let mut changed = false;
        if p.scan.is_none_or(|t| t.elapsed().as_millis() >= 50) {
            p.scan = Some(Instant::now());
            changed |= sync_pops(&mut p.list, shell_pops_now(), winrt_device, &p.tx);
            part(t0, "pops");
        }
        take_opened(&mut p.list, &p.rx);
        changed |= p.grab_gpu(device, context, d2d, winrt_device);
        changed |= p.made(d2d);
        part(t0, "grab");
        let Some(comp) = p.gpu.as_mut().filter(|g| g.size == size) else {
            p.make(device, size.0, size.1);
            return Ok(None);
        };
        // A fresh base is filled with the next frames; until then it is left as it came.
        let mut base_new = std::mem::take(&mut comp.fresh);
        let mut composed = None;
        if let Some((frame, tex, w, h, t)) = cap.next_frame()? {
            let at = POINT {
                x: -c.left,
                y: -c.top,
            };
            place(context, &comp.base, size, &tex, (w, h), at);
            drop(frame);
            cap.follow_size(winrt_device)?;
            (composed, base_new) = (t, true);
        }
        part(t0, "frame");
        if let Some(t) = &mut d.tray {
            if let Some((frame, tex, w, h, tc)) = t.next_frame()? {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { tex.GetDesc(&mut desc) };
                let (tw, th) = (desc.Width, desc.Height);
                let fits = d.tray_tex.as_ref().is_some_and(|(t, _, _)| {
                    let mut td = D3D11_TEXTURE2D_DESC::default();
                    unsafe { t.GetDesc(&mut td) };
                    (td.Width, td.Height) == (tw, th)
                });
                if !fits {
                    d.tray_tex = make_tex(device, tw, th, 0).map(|t| (t, w, h));
                }
                if let Some((tt, cw, ch)) = &mut d.tray_tex {
                    unsafe { context.CopySubresourceRegion(&*tt, 0, 0, 0, 0, &tex, 0, Some(&full(w, h))) };
                    (*cw, *ch) = (w, h);
                }
                drop(frame);
                t.follow_size(winrt_device)?;
                composed = composed.or(tc);
                base_new = true;
            }
        }
        // The taskbar goes over every new desktop frame.
        if base_new {
            if let Some((tt, w, h)) = &d.tray_tex {
                place(context, &comp.base, size, tt, (*w, *h), l.tray_at);
            }
        }
        part(t0, "tray");
        if !(changed || base_new) {
            return Ok(None);
        }
        let comp = comp.comp.clone();
        let r = p.draw(context, d2d);
        part(t0, "draw");
        if let Err(e) = r {
            crate::store::log(&format!("live: desktop not drawn on the GPU ({e}), it goes through the CPU"));
            (p.gpu, p.failed) = (None, true);
            p.list.clear();
            (s.shared_tex, s.shared_back) = (None, None);
            s.slot_size = (0, 0);
            return Ok(None);
        }
        let (Some(sh), Some(dst)) = (shared.as_mut(), s.shared_back.as_ref().map(|b| b.0.clone())) else {
            return Ok(None);
        };
        let r = sh.copy(&dst, &comp, size.0, size.1).and_then(|()| Self::swap(sh, s, renderer));
        part(t0, "copy");
        if let Err(e) = r {
            crate::store::log(&format!("live: shared copy failed ({e}), frames go through the CPU"));
            *shared = None;
            (s.shared_tex, s.shared_back) = (None, None);
            s.slot_size = (0, 0);
            return Ok(None);
        }
        Ok(Some(composed))
    }

    fn poll_cpu(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        winrt_device: &IDirect3DDevice,
        top: Option<isize>,
        s: &mut Stream,
        renderer: &mut Renderer,
    ) -> Result<Option<Option<i64>>> {
        let Stream {
            cap,
            pops,
            desktop,
            slot,
            slot_size,
            upload_ms,
            making,
            held,
            ..
        } = s;
        // A texture made for a new size: the frame held for it goes in first.
        if let Some((w, h, rx)) = making {
            if let Ok(tex) = rx.try_recv() {
                *slot = Some(renderer.live_slot_with(*slot, tex));
                *slot_size = (*w, *h);
                if !held.is_empty() {
                    renderer.write_live(slot.unwrap_or_default(), held, *w * 4, *w, *h);
                }
                held.clear();
                *making = None;
            }
        }
        let mut upload = |data: &[u8], pitch: u32, w: u32, h: u32| {
            let t = Instant::now();
            if slot.is_none() || *slot_size != (w, h) {
                // Making a big texture takes tens of milliseconds: on a worker, holding the frame.
                if !matches!(making, Some((mw, mh, _)) if (*mw, *mh) == (w, h)) {
                    let (tx, rx) = channel();
                    let maker = renderer.texture_maker();
                    std::thread::spawn(move || {
                        let _ = tx.send(maker.make(w, h));
                    });
                    *making = Some((w, h, rx));
                }
                pack(held, data, pitch, w, h);
            } else if let Some(slot) = *slot {
                renderer.write_live(slot, data, pitch, w, h);
            }
            *upload_ms = t.elapsed().as_secs_f32() * 1000.0;
        };
        let composed = match desktop {
            // The window in use: its popups are drawn over its frames.
            None if top.is_some() || !pops.list.is_empty() => {
                let mut changed = pops.scan(top, winrt_device);
                changed |= grab_pops(&mut pops.list, device, context, winrt_device).is_some();
                let main = cap.grab(device, context, winrt_device, |data, pitch, w, h| {
                    pack(&mut pops.px, data, pitch, w, h);
                    pops.size = (w, h);
                    pops.fresh = true;
                })?;
                match main.or(changed.then_some(None)) {
                    Some(composed) if pops.fresh => {
                        pops.out.clone_from(&pops.px);
                        for p in pops.list.iter().rev() {
                            blend(&mut pops.out, pops.size, &p.px, p.size, p.at);
                        }
                        let (w, h) = pops.size;
                        upload(&pops.out, w * 4, w, h);
                        Some(composed)
                    }
                    _ => None,
                }
            }
            None => {
                pops.fresh = false;
                cap.grab(device, context, winrt_device, upload)?
            }
            Some(d) => {
                let layout = crate::win::desktop_layout(d.progman);
                let desk = match &layout {
                    Some(l) => cap.grab(device, context, winrt_device, |data, pitch, w, h| {
                        d.crop(l.crop, data, pitch, w, h)
                    })?,
                    None => None,
                };
                let tray = match &mut d.tray {
                    Some(t) => t.grab(device, context, winrt_device, |data, pitch, w, h| {
                        pack(&mut d.tray_px, data, pitch, w, h);
                        d.tray_size = (w, h);
                    })?,
                    None => None,
                };
                let pops = d.poll_pops(device, context, winrt_device);
                match (&layout, desk.or(tray).or(pops)) {
                    (Some(l), Some(composed)) if !d.desk.is_empty() => {
                        d.out.clone_from(&d.desk);
                        d.draw_tray(l.tray_at);
                        for p in d.pops.iter().rev() {
                            blend(&mut d.out, d.out_size, &p.px, p.size, p.at);
                        }
                        let (w, h) = d.out_size;
                        upload(&d.out, w * 4, w, h);
                        Some(composed)
                    }
                    _ => None,
                }
            }
        };
        Ok(composed)
    }
}

fn make_staging(device: &ID3D11Device, w: u32, h: u32) -> Option<ID3D11Texture2D> {
    let sd = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut st = None;
    unsafe { device.CreateTexture2D(&sd, None, Some(&mut st)).ok()? };
    st
}

/// Copies a frame's rows into `dst`, tightly packed.
fn pack(dst: &mut Vec<u8>, data: &[u8], pitch: u32, w: u32, h: u32) {
    dst.clear();
    for y in 0..h {
        let at = (y * pitch) as usize;
        dst.extend_from_slice(&data[at..at + (w * 4) as usize]);
    }
}

/// Draws premultiplied BGRA `px` (`size`) over `out` (`out_size`) at `at`, clipped to it.
fn blend(out: &mut [u8], out_size: (u32, u32), px: &[u8], size: (u32, u32), at: POINT) {
    let ((ow, oh), (w, h)) = (out_size, size);
    if px.len() < (w * h * 4) as usize || out.len() < (ow * oh * 4) as usize {
        return;
    }
    let (x0, x1) = (at.x.max(0), (at.x + w as i32).min(ow as i32));
    for y in at.y.max(0)..(at.y + h as i32).min(oh as i32) {
        for x in x0..x1 {
            let s = (((y - at.y) as u32 * w + (x - at.x) as u32) * 4) as usize;
            let a = px[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = ((y as u32 * ow + x as u32) * 4) as usize;
            for c in 0..3 {
                let v = px[s + c] as u32 + out[d + c] as u32 * (255 - a) / 255;
                out[d + c] = v.min(255) as u8;
            }
        }
    }
}

/// Keeps `pops` in step with the popups open now (`now`: each one's window and top left, in
/// order), opening captures of new ones on worker threads (their results come through `tx`).
/// Returns whether any came, went or moved.
fn sync_pops(
    pops: &mut Vec<Pop>,
    now: Vec<(isize, POINT)>,
    winrt_device: &IDirect3DDevice,
    tx: &Sender<(isize, Result<Capture>)>,
) -> bool {
    let before = pops.len();
    pops.retain(|p| {
        let keep = now.iter().any(|(h, _)| *h == p.hwnd);
        if !keep {
            crate::win::trace_pop(p.hwnd, "card", "gone from the card");
        }
        keep
    });
    let mut changed = pops.len() != before;
    let order: Vec<isize> = pops.iter().map(|p| p.hwnd).collect();
    let mut next = Vec::with_capacity(now.len());
    for (h, at) in now {
        let p = match pops.iter().position(|p| p.hwnd == h) {
            Some(i) => pops.swap_remove(i),
            None => {
                crate::win::trace_pop(h, "card", &format!("on the card at ({},{}): capture opening", at.x, at.y));
                let (device, tx) = (AgileDevice(winrt_device.clone()), tx.clone());
                std::thread::spawn(move || {
                    let device = device;
                    unsafe {
                        let _ = windows::Win32::System::Com::CoInitializeEx(
                            None,
                            windows::Win32::System::Com::COINIT_MULTITHREADED,
                        );
                    }
                    let _ = tx.send((h, open_capture(&device.0, h)));
                });
                Pop {
                    hwnd: h,
                    at,
                    cap: None,
                    px: Vec::new(),
                    size: (0, 0),
                    tex: None,
                }
            }
        };
        if p.at.x != at.x || p.at.y != at.y {
            changed = true;
            crate::win::trace_pop(h, "card", &format!("moved on the card to ({},{})", at.x, at.y));
        }
        next.push(Pop { at, ..p });
    }
    // The same popups stacked differently (one raised over another).
    changed |= order.iter().copied().ne(next.iter().map(|p| p.hwnd).filter(|h| order.contains(h)));
    *pops = next;
    changed
}

/// Takes in the popup captures opened since the last call.
fn take_opened(pops: &mut [Pop], rx: &Receiver<(isize, Result<Capture>)>) {
    while let Ok((h, cap)) = rx.try_recv() {
        match (pops.iter_mut().find(|p| p.hwnd == h), cap) {
            (Some(p), Ok(cap)) => {
                crate::win::trace_pop(h, "card", "capture opened");
                p.cap = Some(cap);
            }
            (Some(_), Err(e)) => crate::win::trace_pop(h, "card", &format!("capture failed: {e}")),
            (None, _) => crate::win::trace_pop(h, "card", "capture opened after it was gone"),
        }
    }
}

/// Takes the newest frame of each popup into its pixels. Returns Some when one changed.
fn grab_pops(
    pops: &mut [Pop],
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    winrt_device: &IDirect3DDevice,
) -> Option<Option<i64>> {
    let mut changed = None;
    for p in pops {
        let Some(cap) = &mut p.cap else { continue };
        let first = p.size == (0, 0);
        match cap.grab(device, context, winrt_device, |data, pitch, w, h| {
            pack(&mut p.px, data, pitch, w, h);
            p.size = (w, h);
        }) {
            Ok(Some(c)) => {
                if first {
                    crate::win::trace_pop(p.hwnd, "frame", &format!("first frame on the card, {}x{}", p.size.0, p.size.1));
                }
                changed = Some(changed.flatten().or(c))
            }
            Ok(None) => {}
            Err(e) => {
                crate::win::trace_pop(p.hwnd, "card", &format!("capture lost: {e}"));
                p.cap = None
            }
        }
    }
    changed
}

/// Copies the top-left `size` of `src` into `dst` (`dst_size`) with its top left at `at`,
/// clipped to `dst`.
fn place(
    context: &ID3D11DeviceContext,
    dst: &ID3D11Texture2D,
    (dw, dh): (u32, u32),
    src: &ID3D11Texture2D,
    (sw, sh): (u32, u32),
    at: POINT,
) {
    let (x0, y0) = (at.x.max(0), at.y.max(0));
    let (x1, y1) = ((at.x + sw as i32).min(dw as i32), (at.y + sh as i32).min(dh as i32));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let b = D3D11_BOX {
        left: (x0 - at.x) as u32,
        top: (y0 - at.y) as u32,
        front: 0,
        right: (x1 - at.x) as u32,
        bottom: (y1 - at.y) as u32,
        back: 1,
    };
    unsafe { context.CopySubresourceRegion(dst, 0, x0 as u32, y0 as u32, 0, src, 0, Some(&b)) };
}

/// The shell popups open now with their top lefts relative to the primary monitor (the
/// desktop card's), topmost first. One of a process out of reach (a tray menu of Task Manager
/// or another app run as administrator) can't be hidden or put under the 3D view, so it shows
/// on the real screen over the 3D view: drawn on the card too, it showed twice.
fn shell_pops_now() -> Vec<(isize, POINT)> {
    let m = crate::win::primary_rect().unwrap_or_default();
    crate::win::shell_popups()
        .into_iter()
        .filter(|&(h, _)| !crate::win::elevated(crate::win::hwnd_of(h)))
        .map(|(h, r)| {
            let at = POINT {
                x: r.left - m.left,
                y: r.top - m.top,
            };
            (h, at)
        })
        .collect()
}

/// The box of the top-left `w`x`h` of a texture.
fn full(w: u32, h: u32) -> D3D11_BOX {
    D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: w,
        bottom: h,
        back: 1,
    }
}

/// A Direct2D context on `device`, drawing in pixels.
fn make_d2d(device: &ID3D11Device) -> Option<ID2D1DeviceContext> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    unsafe {
        let d2d = D2D1CreateDevice(&dxgi, None).ok()?;
        let ctx = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE).ok()?;
        ctx.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
        Some(ctx)
    }
}

/// A `w`x`h` BGRA texture on the GPU, with `bind` flags.
fn make_tex(device: &ID3D11Device, w: u32, h: u32, bind: u32) -> Option<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)).ok()? };
    t
}

/// Texture `tex` as a Direct2D bitmap (premultiplied alpha).
fn bitmap(
    d2d: &ID2D1DeviceContext,
    tex: &ID3D11Texture2D,
    options: D2D1_BITMAP_OPTIONS,
) -> Result<ID2D1Bitmap1> {
    let surface: IDXGISurface = tex.cast()?;
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: options,
        colorContext: core::mem::ManuallyDrop::new(None),
    };
    unsafe { d2d.CreateBitmapFromDxgiSurface(&surface, Some(&props)) }
}

impl Popups {
    /// Keeps the list in step with the popups of window `top` open now (looked for every 50
    /// ms); with no window, empties it. Returns whether the popups came, went or moved.
    fn scan(&mut self, top: Option<isize>, winrt_device: &IDirect3DDevice) -> bool {
        let Some(top) = top else {
            let had = !self.list.is_empty();
            self.list.clear();
            return had;
        };
        let mut changed = false;
        if self.scan.is_none_or(|t| t.elapsed().as_millis() >= 50) {
            self.scan = Some(Instant::now());
            changed = sync_pops(&mut self.list, crate::win::popups_of(top), winrt_device, &self.tx);
        }
        take_opened(&mut self.list, &self.rx);
        changed
    }

    /// Takes the newest frame of each popup into its texture. Returns whether one changed.
    fn grab_gpu(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        d2d: &ID2D1DeviceContext,
        winrt_device: &IDirect3DDevice,
    ) -> bool {
        let mut changed = false;
        for p in &mut self.list {
            let Some(cap) = &mut p.cap else { continue };
            match cap.next_frame() {
                Ok(Some((frame, tex, w, h, _))) => {
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe { tex.GetDesc(&mut desc) };
                    let (tw, th) = (desc.Width, desc.Height);
                    if !p.tex.as_ref().is_some_and(|t| (t.w, t.h) == (tw, th)) {
                        let bind = D3D11_BIND_SHADER_RESOURCE.0 as u32;
                        p.tex = make_tex(device, tw, th, bind).and_then(|tex| {
                            let bmp = bitmap(d2d, &tex, D2D1_BITMAP_OPTIONS_NONE).ok()?;
                            Some(PopTex { tex, bmp, w: tw, h: th })
                        });
                    }
                    if let Some(t) = &p.tex {
                        unsafe {
                            context.CopySubresourceRegion(&t.tex, 0, 0, 0, 0, &tex, 0, Some(&full(w, h)))
                        };
                        if p.size == (0, 0) {
                            crate::win::trace_pop(p.hwnd, "frame", &format!("first frame on the card, {w}x{h}"));
                        }
                        p.size = (w, h);
                        changed = true;
                    }
                    drop(frame);
                    if cap.follow_size(winrt_device).is_err() {
                        p.cap = None;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    crate::win::trace_pop(p.hwnd, "card", &format!("capture lost: {e}"));
                    p.cap = None
                }
            }
        }
        changed
    }

    /// Starts making the textures to draw the popups over `w`x`h` frames on a worker.
    fn make(&mut self, device: &ID3D11Device, w: u32, h: u32) {
        if self.failed || matches!(&self.making, Some((mw, mh, _)) if (*mw, *mh) == (w, h)) {
            return;
        }
        let (tx, rx) = channel();
        let device = Agile(device.clone());
        std::thread::spawn(move || {
            let device = device;
            let sr = D3D11_BIND_SHADER_RESOURCE.0 as u32;
            let made = make_tex(&device.0, w, h, sr)
                .zip(make_tex(&device.0, w, h, sr | D3D11_BIND_RENDER_TARGET.0 as u32));
            let _ = tx.send(made.map(Agile));
        });
        self.making = Some((w, h, rx));
    }

    /// Takes in the textures made by `make`. Returns true when they are ready.
    fn made(&mut self, d2d: &ID2D1DeviceContext) -> bool {
        let Some((w, h, rx)) = &self.making else {
            return false;
        };
        let size = (*w, *h);
        let made = match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Ok(Some(Agile((base, comp)))) => {
                let options = D2D1_BITMAP_OPTIONS(
                    D2D1_BITMAP_OPTIONS_TARGET.0 | D2D1_BITMAP_OPTIONS_CANNOT_DRAW.0,
                );
                bitmap(d2d, &comp, options).ok().map(|target| Compose {
                    base,
                    comp,
                    target,
                    size,
                    fresh: true,
                })
            }
            _ => None,
        };
        self.making = None;
        if made.is_none() {
            crate::store::log("live: popups of the window in use cannot be drawn on its card");
            self.failed = true;
        }
        self.gpu = made;
        self.gpu.is_some()
    }

    /// Draws the window's frame with the popups over it into `comp`.
    fn draw(&self, context: &ID3D11DeviceContext, d2d: &ID2D1DeviceContext) -> Result<()> {
        let Some(c) = &self.gpu else {
            return Ok(());
        };
        unsafe {
            context.CopyResource(&c.comp, &c.base);
            d2d.SetTarget(&c.target);
            d2d.BeginDraw();
            for p in self.list.iter().rev() {
                let (Some(t), (w, h)) = (&p.tex, p.size) else { continue };
                let (x, y) = (p.at.x as f32, p.at.y as f32);
                let dst = D2D_RECT_F {
                    left: x,
                    top: y,
                    right: x + w as f32,
                    bottom: y + h as f32,
                };
                let src = D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: w as f32,
                    bottom: h as f32,
                };
                let nearest = D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR;
                d2d.DrawBitmap(&t.bmp, Some(&dst), 1.0, nearest, Some(&src), None);
            }
            let r = d2d.EndDraw(None, None);
            d2d.SetTarget(None::<&ID2D1Image>);
            r
        }
    }
}

impl Desktop {
    /// Keeps the popup captures in step with the shell popups open now (looked for every
    /// 100 ms; captures open on worker threads) and takes their newest frames. Returns
    /// Some when the picture changed.
    fn poll_pops(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        winrt_device: &IDirect3DDevice,
    ) -> Option<Option<i64>> {
        let mut changed = None;
        if self.pop_scan.is_none_or(|t| t.elapsed().as_millis() >= 100) {
            self.pop_scan = Some(Instant::now());
            if sync_pops(&mut self.pops, shell_pops_now(), winrt_device, &self.pop_tx) {
                changed = Some(None);
            }
        }
        take_opened(&mut self.pops, &self.pop_rx);
        if let Some(c) = grab_pops(&mut self.pops, device, context, winrt_device) {
            changed = Some(changed.flatten().or(c));
        }
        changed
    }

    /// Copies the `c` part (the primary monitor) of a Progman frame into `desk`; what the frame
    /// doesn't cover stays black.
    fn crop(&mut self, c: RECT, data: &[u8], pitch: u32, w: u32, h: u32) {
        let (cw, ch) = ((c.right - c.left).max(1), (c.bottom - c.top).max(1));
        self.desk.clear();
        self.desk.resize((cw * ch * 4) as usize, 0);
        self.out_size = (cw as u32, ch as u32);
        let (x0, x1) = (c.left.max(0), c.right.min(w as i32));
        if x1 <= x0 {
            return;
        }
        let n = ((x1 - x0) * 4) as usize;
        for y in c.top.max(0)..c.bottom.min(h as i32) {
            let src = (y as u32 * pitch + x0 as u32 * 4) as usize;
            let dst = (((y - c.top) * cw + (x0 - c.left)) * 4) as usize;
            self.desk[dst..dst + n].copy_from_slice(&data[src..src + n]);
        }
    }

    /// Draws the latest taskbar frame over `out` at `at` (clipped to it).
    fn draw_tray(&mut self, at: POINT) {
        let ((ow, oh), (tw, th)) = (self.out_size, self.tray_size);
        if self.tray_px.len() < (tw * th * 4) as usize {
            return;
        }
        let (x0, x1) = (at.x.max(0), (at.x + tw as i32).min(ow as i32));
        if x1 <= x0 {
            return;
        }
        let n = ((x1 - x0) * 4) as usize;
        for y in at.y.max(0)..(at.y + th as i32).min(oh as i32) {
            let src = (((y - at.y) as u32 * tw + (x0 - at.x) as u32) * 4) as usize;
            let dst = ((y as u32 * ow + x0 as u32) * 4) as usize;
            self.out[dst..dst + n].copy_from_slice(&self.tray_px[src..src + n]);
        }
    }
}

impl Capture {
    /// Takes the newest frame, if one arrived, and hands its pixels (row pitch, width, height)
    /// to `f`. Returns Some(the time Windows composed it) for a new frame.
    fn grab(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        winrt_device: &IDirect3DDevice,
        f: impl FnOnce(&[u8], u32, u32, u32),
    ) -> Result<Option<Option<i64>>> {
        // The copy made on the last poll is read first, unless the GPU is still at it.
        let mut read = None;
        if let (Some((w, h, composed)), Some((staging, _, _))) = (self.copied, &self.staging) {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            match unsafe {
                context.Map(
                    staging,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            } {
                Ok(()) => {
                    let len = (mapped.RowPitch * (h - 1) + w * 4) as usize;
                    let data =
                        unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, len) };
                    f(data, mapped.RowPitch, w, h);
                    unsafe { context.Unmap(staging, 0) };
                    self.copied = None;
                    read = Some(composed);
                }
                Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => return Ok(None),
                Err(e) => return Err(e),
            }
        }

        let Some((frame, tex, w, h, composed)) = self.next_frame()? else {
            return Ok(read);
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };

        if !matches!(&self.staging, Some((_, sw, sh)) if *sw == desc.Width && *sh == desc.Height) {
            let size = (desc.Width, desc.Height);
            if !matches!(&self.making, Some((w, h, _)) if (*w, *h) == size) {
                let (tx, rx) = channel();
                let device = Agile(device.clone());
                std::thread::spawn(move || {
                    let device = device;
                    let _ = tx.send(make_staging(&device.0, size.0, size.1).map(Agile));
                });
                self.making = Some((size.0, size.1, rx));
            }
            match self.making.as_ref().map(|m| m.2.try_recv()) {
                Some(Ok(st)) => {
                    self.making = None;
                    self.staging = st.map(|t| (t.0, size.0, size.1));
                }
                Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {
                    self.waiting = Some(frame);
                    return Ok(read);
                }
                _ => self.making = None,
            }
        }
        let Some((staging, _, _)) = &self.staging else {
            return Ok(read);
        };
        unsafe {
            context.CopyResource(staging, &tex);
            context.Flush();
        }
        drop(frame);
        self.copied = Some((w, h, composed));
        self.follow_size(winrt_device)?;
        Ok(read)
    }

    /// Drains to the newest frame (or the one held back for a texture being made). Returns it
    /// with its texture, the size of the window content in it and the time Windows composed it.
    #[allow(clippy::type_complexity)]
    fn next_frame(
        &mut self,
    ) -> Result<Option<(Direct3D11CaptureFrame, ID3D11Texture2D, u32, u32, Option<i64>)>> {
        let mut newest = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            newest = Some(f);
        }
        let Some(frame) = newest.or_else(|| self.waiting.take()) else {
            return Ok(None);
        };
        let composed = frame.SystemRelativeTime().map(|t| t.Duration).ok();
        let content = frame.ContentSize()?;
        let surface = frame.Surface()?;
        let tex: ID3D11Texture2D = unsafe {
            surface
                .cast::<IDirect3DDxgiInterfaceAccess>()?
                .GetInterface()?
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        // A frame left over from before the last resize (its surface still has the old pool
        // size) would undo the resize: a window restored from minimized would stay captured at
        // its minimized size.
        if desc.Width != self.size.Width as u32 || desc.Height != self.size.Height as u32 {
            return Ok(None);
        }
        self.resized = None;
        self.content = content;
        let (w, h) = (
            (content.Width.max(1) as u32).min(desc.Width),
            (content.Height.max(1) as u32).min(desc.Height),
        );
        Ok(Some((frame, tex, w, h, composed)))
    }

    /// After a frame was used: if the window was resized, capture at the new size from now on.
    fn follow_size(&mut self, winrt_device: &IDirect3DDevice) -> Result<()> {
        let content = self.content;
        if content.Width != self.size.Width || content.Height != self.size.Height {
            self.size = content;
            self.resized = Some(Instant::now());
            self.pool.Recreate(
                winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                content,
            )?;
        }
        Ok(())
    }
}
