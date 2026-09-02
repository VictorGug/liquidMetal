//! Windows side of the overlay: DWM composition and an extended window style.
//!
//! The third of three. `overlay_x11.rs` and `overlay_mac.rs` are the others, and all
//! three provide the same items to `main.rs` so the frame loop never learns which
//! platform it is on.
//!
//! # The three things that make a window an overlay, and how Win32 spells them
//!
//! | | X11 | macOS | Windows |
//! | --- | --- | --- | --- |
//! | transparent | a depth-32 ARGB visual | `setOpaque:NO` + surface opacity 0 | DWM blur-behind with an **empty** blur region |
//! | always on top | `_NET_WM_STATE_ABOVE` | `setLevel:` | `WS_EX_TOPMOST` |
//! | click-through except on the blob | XShape `ShapeInput` | toggle `ignoresMouseEvents` | toggle `WS_EX_TRANSPARENT` |
//!
//! # Transparency: an empty blur region is the whole trick
//!
//! `DwmEnableBlurBehindWindow` sounds like the wrong function, and the name is a
//! historical accident. What it does that matters here is switch the window onto
//! DWM's *per-pixel alpha* path — after which the alpha the shader writes is the
//! alpha the desktop composites with. The blur is a separate thing, requested by the
//! region you hand it, and Windows 8 dropped it anyway.
//!
//! So the region is deliberately **empty** — `CreateRectRgn(0, 0, -1, -1)`, a
//! rectangle with negative extent. Blur nothing, but respect alpha everywhere. Pass
//! a null region instead and you ask for the whole window to be blurred, which is
//! not wanted and on Windows 8 and later does nothing at all.
//!
//! This replaces `UpdateLayeredWindow`, which is the other way to get per-pixel
//! alpha on Windows and cannot be used here: it wants to be handed a bitmap, and
//! there is no bitmap — the frame lives in an OpenGL back buffer that
//! `SwapBuffers` presents.
//!
//! # Click-through works the same way as on macOS
//!
//! Windows has `SetWindowRgn`, which is a real region and does per-pixel hit
//! testing — but like X11's `ShapeBounding` it clips the *rendering* too, which
//! would hard-edge the antialiased rim. So it is not used, and the region is
//! emulated exactly as on macOS: every frame the global cursor is tested against the
//! same rectangle cover `physics::hit_rects` builds, and `WS_EX_TRANSPARENT` is
//! turned on or off to match.
//!
//! The same two consequences follow, and are handled the same way: the decision is
//! one frame stale, so the local hit test stays on; and a window with
//! `WS_EX_TRANSPARENT` receives no mouse messages at all, so nothing can wake an
//! idle overlay and it has to poll.

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::Graphics::Dwm::{
    DWM_BB_BLURREGION, DWM_BB_ENABLE, DWM_BLURBEHIND, DwmEnableBlurBehindWindow,
};
use windows_sys::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetCursorPos, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
};

// ---------------------------------------------------------------------------
// TUNABLES
// ---------------------------------------------------------------------------

/// Extended styles applied once, and then left alone.
///
/// `TOPMOST` floats above ordinary windows. `TOOLWINDOW` keeps us out of the taskbar
/// and out of Alt+Tab, which is the Win32 equivalent of the `_SKIP_TASKBAR` and
/// `_SKIP_PAGER` hints the X11 build sets. `NOACTIVATE` means clicking the blob does
/// not yank focus away from whatever you were working in — the one thing the macOS
/// build cannot do and has to document as a wart.
///
/// Deliberately **not** `WS_EX_LAYERED`. A layered window gets its pixels from
/// `UpdateLayeredWindow` or a uniform alpha, neither of which an OpenGL swap chain
/// can provide, and on some drivers combining it with GL gives a window that never
/// paints at all. DWM does the compositing instead.
const EX_STYLE_ON: isize =
    (WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as isize;

/// How long after the window is shown to keep re-asserting the DWM state, in
/// seconds, and how often within that.
///
/// The macOS build learned this the hard way: composition settings applied before a
/// window has actually been presented can apply to nothing. Windows is better
/// behaved, but the same shape of bug is cheap to rule out and miserable to diagnose
/// remotely, so the same schedule is used.
const REASSERT_AT_SECS: [f32; 5] = [0.2, 0.5, 1.0, 2.0, 4.0];

// ---------------------------------------------------------------------------
// The surface every platform's overlay has to provide.
// ---------------------------------------------------------------------------

/// What `main.rs` calls the thing that holds the window on the desktop.
pub type Overlay = Win;

/// Tell SDL to be per-monitor DPI aware before it initialises.
///
/// Without this Windows lies to a process about the size of the desktop: on a
/// display scaled to 150% it reports 1280x720 for a 1920x1080 panel and then
/// stretches everything the process draws. The overlay would cover two thirds of the
/// screen and the blob would be soft. This has to be set before `SDL_Init`, because
/// SDL reads it while bringing the video subsystem up.
pub fn prepare_process_environment() {
    sdl2::hint::set("SDL_WINDOWS_DPI_AWARENESS", "permonitorv2");
}

/// Frame budget while the blob is asleep: ~33 fps.
///
/// Same reasoning as macOS. A window with `WS_EX_TRANSPARENT` receives no mouse
/// messages, so the event queue cannot tell us the pointer has arrived over the
/// blob; the only way to find out is to look, and this is how often.
pub const IDLE_WAIT_MS: u32 = 30;

/// There is no input region, so the local hit test is not redundant here.
pub const REGION_IS_THE_HIT_TEST: bool = false;

/// DWM composites every window; there is no visual to choose and nothing to retry.
pub const NEEDS_VISUAL_STRATEGY: bool = false;

pub const REQUIRED_VIDEO_DRIVER: &str = "windows";

pub fn check_video_driver(driver: &str) -> Result<(), String> {
    if driver == REQUIRED_VIDEO_DRIVER {
        return Ok(());
    }
    Err(format!(
        "SDL chose the {driver:?} video driver, but the Windows overlay needs \
         {REQUIRED_VIDEO_DRIVER:?}.\n\
         The overlay calls Win32 functions on the HWND behind the SDL window, which \
         only exists on the Windows backend."
    ))
}

/// Pull the native window handle out of SDL: on Windows, the `HWND`.
///
/// Read out of the union's byte array rather than a `win` field, because there is no
/// such field to read — `sdl2-sys` ships one pre-generated `sdl_bindings.rs` for
/// every platform and it was generated on Linux, so the `SDL_SysWMinfo` union it
/// declares has `x11`, `wl` and `dummy` and nothing else.
///
/// That is safe here for the same reason it is safe on macOS: `SDL_syswm.h` pads the
/// union with `Uint8 dummy[64]` so its size does not vary, and the Windows arm
/// begins `struct { HWND window; HDC hdc; ... }` — a pointer at offset zero. Reading
/// the first eight bytes of `dummy` is exactly reading `info.info.win.window`, and
/// reading the `dummy` arm of a union is always valid because `u8` has no invalid bit
/// patterns.
pub fn native_window_handle(window: &sdl2::video::Window) -> Result<u64, String> {
    // SAFETY: `info` is a plain C struct with no invalid bit patterns for the fields
    // SDL reads, the version is stamped before the call as SDL requires, and
    // `window.raw()` is a live SDL_Window for the lifetime of the borrow.
    unsafe {
        let mut info: sdl2::sys::SDL_SysWMinfo = std::mem::zeroed();
        sdl2::sys::SDL_GetVersion(&mut info.version);
        if sdl2::sys::SDL_GetWindowWMInfo(window.raw(), &mut info) != sdl2::sys::SDL_bool::SDL_TRUE
        {
            return Err(format!(
                "SDL_GetWindowWMInfo failed: {}\n\
                 Without the HWND the overlay cannot make itself transparent, float \
                 above other windows, or let clicks through.",
                sdl2::get_error()
            ));
        }
        if info.subsystem != sdl2::sys::SDL_SYSWM_TYPE::SDL_SYSWM_WINDOWS {
            return Err(format!(
                "the SDL window is not a Win32 window (SDL_SYSWM_TYPE = {}).",
                info.subsystem as u32
            ));
        }
        let mut ptr = [0u8; 8];
        ptr.copy_from_slice(&info.info.dummy[..8]);
        let handle = u64::from_ne_bytes(ptr);
        if handle == 0 {
            return Err("SDL reported a Win32 window with a null HWND".into());
        }
        Ok(handle)
    }
}

// ---------------------------------------------------------------------------

/// One display, for the startup log.
#[derive(Clone, Debug)]
pub struct DisplayInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

pub struct Win {
    hwnd: HWND,
    /// The union of every display, in SDL's global desktop coordinates. May be
    /// negative: a monitor left of or above the primary one starts below zero.
    ///
    /// As on macOS, everything `main.rs` sees is relative to this rectangle, which is
    /// why `window_rect` reports (0, 0): the window covers the desktop exactly, so
    /// window-relative *is* desktop-relative, and there is no window manager to argue
    /// with about it.
    origin: (i32, i32),
    size: (u32, u32),
    displays: Vec<DisplayInfo>,
    click_through: bool,
    rects: Vec<(i32, i32, i32, i32)>,
    /// `Ok` once DWM has accepted the per-pixel alpha request, `Err` with the
    /// HRESULT if it refused. Reported in the startup log either way.
    composition: Result<(), String>,
    /// How many of `REASSERT_AT_SECS` have been done.
    reasserts_done: usize,
    started: Option<std::time::Instant>,
    /// There is no visual to choose on Windows. Present only because `main.rs` reads
    /// it on every platform.
    pub argb_visual: Option<u32>,
}

impl Win {
    /// Nothing to connect to. Displays cannot be enumerated until SDL's video
    /// subsystem is up, which is what `probe_desktop` is for.
    pub fn connect() -> Result<Win, String> {
        Ok(Win {
            hwnd: std::ptr::null_mut(),
            origin: (0, 0),
            size: (0, 0),
            displays: Vec::new(),
            click_through: true,
            rects: Vec::new(),
            composition: Err("not attempted yet".into()),
            reasserts_done: 0,
            started: None,
            argb_visual: None,
        })
    }

    /// The union of every attached display.
    ///
    /// Asked of SDL rather than of `EnumDisplayMonitors`, so the coordinates are in
    /// the same space SDL will place the window in and there is no chance of getting
    /// a DPI-scaled rectangle right in one place and wrong in another.
    pub fn probe_desktop(&mut self, video: &sdl2::VideoSubsystem) -> Result<(), String> {
        let n = video
            .num_video_displays()
            .map_err(|e| format!("SDL could not enumerate the displays: {e}"))?;
        if n <= 0 {
            return Err("SDL reports no displays at all".into());
        }
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        self.displays.clear();
        for i in 0..n {
            let r = video
                .display_bounds(i)
                .map_err(|e| format!("SDL could not read the bounds of display {i}: {e}"))?;
            x0 = x0.min(r.x());
            y0 = y0.min(r.y());
            x1 = x1.max(r.x() + r.width() as i32);
            y1 = y1.max(r.y() + r.height() as i32);
            self.displays.push(DisplayInfo {
                name: video.display_name(i).unwrap_or_else(|_| format!("display {i}")),
                x: r.x(),
                y: r.y(),
                width: r.width(),
                height: r.height(),
                primary: i == 0,
            });
        }
        self.origin = (x0, y0);
        self.size = ((x1 - x0).max(1) as u32, (y1 - y0).max(1) as u32);
        Ok(())
    }

    pub fn desktop_size(&self) -> (u32, u32) {
        self.size
    }

    pub fn desktop_origin(&self) -> (i32, i32) {
        self.origin
    }

    pub fn set_window(&mut self, handle: u64) -> Result<(), String> {
        if handle == 0 {
            return Err("the HWND is null".into());
        }
        self.hwnd = handle as HWND;
        Ok(())
    }

    fn win(&self) -> Result<HWND, String> {
        if self.hwnd.is_null() {
            return Err("the overlay has no HWND yet".into());
        }
        Ok(self.hwnd)
    }

    fn ex_style(&self) -> isize {
        // SAFETY: live HWND; GWL_EXSTYLE is a documented index and the call has no
        // preconditions beyond that.
        unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) }
    }

    fn set_ex_style(&self, style: isize) {
        // SAFETY: as above; the value is the style word we just read, modified.
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style);
        }
    }

    /// Ask DWM to composite this window with per-pixel alpha.
    ///
    /// The empty blur region is the point; see the module docs. Idempotent, so it is
    /// safe to call again from `on_frame`.
    fn enable_per_pixel_alpha(&self) -> Result<(), String> {
        let hwnd = self.win()?;
        // SAFETY: `CreateRectRgn` returns an owned region or null; `DwmEnableBlurBehindWindow`
        // copies what it needs from the struct and does not take ownership of the
        // region, so it is deleted immediately afterwards on both paths.
        unsafe {
            let region = CreateRectRgn(0, 0, -1, -1);
            let bb = DWM_BLURBEHIND {
                dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
                fEnable: 1,
                hRgnBlur: region,
                fTransitionOnMaximized: 0,
            };
            let hr = DwmEnableBlurBehindWindow(hwnd, &bb);
            if !region.is_null() {
                DeleteObject(region as _);
            }
            if hr < 0 {
                return Err(format!(
                    "DwmEnableBlurBehindWindow failed (HRESULT {hr:#010x}); the overlay \
                     will be an opaque rectangle. Desktop composition has to be on."
                ));
            }
        }
        Ok(())
    }

    /// Float above everything, stay out of the taskbar, do not steal focus, and ask
    /// DWM for per-pixel alpha.
    ///
    /// The DWM call lives here rather than in `on_gl_context_ready` for a boring but
    /// important reason: that hook runs before `set_window`, so there is no HWND yet
    /// to hand to DWM. macOS gets away with doing its transparency work there
    /// because what it needs is the GL context, which does exist by then. This
    /// needs the window, and the window arrives later.
    pub fn apply_overlay_properties(&mut self) -> Result<(), String> {
        self.win()?;
        let style = self.ex_style();
        // Keep whatever SDL set and add ours; never clobber the word wholesale.
        self.set_ex_style(style | EX_STYLE_ON);
        self.composition = self.enable_per_pixel_alpha();
        self.composition.clone()
    }

    /// Start the re-assert clock.
    ///
    /// Nothing to do to the context itself — the alpha bits were settled when SDL
    /// chose the pixel format — and nothing can be done to the window yet, because
    /// this runs before `set_window`. All the real work is in
    /// `apply_overlay_properties`.
    pub fn on_gl_context_ready(&mut self) -> Result<(), String> {
        self.started = Some(std::time::Instant::now());
        Ok(())
    }

    /// Re-assert the DWM state for the first few seconds after start-up.
    ///
    /// See `REASSERT_AT_SECS`. Costs five calls and then nothing at all.
    pub fn on_frame(&mut self) {
        let Some(started) = self.started else { return };
        if self.hwnd.is_null() {
            return;
        }
        if self.reasserts_done >= REASSERT_AT_SECS.len() {
            return;
        }
        let t = started.elapsed().as_secs_f32();
        while self.reasserts_done < REASSERT_AT_SECS.len()
            && t >= REASSERT_AT_SECS[self.reasserts_done]
        {
            self.reasserts_done += 1;
            let r = self.enable_per_pixel_alpha();
            // Only ever upgrade the reported state: a later success means the
            // overlay is transparent now, whatever happened at start-up.
            if r.is_ok() {
                self.composition = Ok(());
            }
        }
    }

    fn set_click_through(&mut self, on: bool) -> Result<(), String> {
        if self.click_through == on {
            return Ok(());
        }
        self.win()?;
        let style = self.ex_style();
        let next = if on {
            style | WS_EX_TRANSPARENT as isize
        } else {
            style & !(WS_EX_TRANSPARENT as isize)
        };
        self.set_ex_style(next);
        self.click_through = on;
        Ok(())
    }

    /// Fully click-through: every pixel belongs to whatever is underneath.
    pub fn set_input_region_empty(&mut self) -> Result<(), String> {
        self.rects.clear();
        self.win()?;
        let style = self.ex_style();
        self.set_ex_style(style | WS_EX_TRANSPARENT as isize);
        self.click_through = true;
        Ok(())
    }

    /// The blob's shape this frame.
    ///
    /// Win32 cannot be told about a shape without also clipping what is drawn, so
    /// this compares the global cursor against the rectangles itself and toggles
    /// `WS_EX_TRANSPARENT`. Returns whether the window's state actually changed,
    /// which is what the X11 version returns too.
    pub fn set_input_rects(&mut self, rects: &[(i32, i32, i32, i32)]) -> Result<bool, String> {
        self.rects.clear();
        self.rects.extend_from_slice(rects);
        let on_blob = match global_cursor() {
            Some((gx, gy)) => {
                // The rectangles are window-relative; the cursor is not.
                let x = gx - self.origin.0;
                let y = gy - self.origin.1;
                rects
                    .iter()
                    .any(|(rx, ry, rw, rh)| x >= *rx && y >= *ry && x < rx + rw && y < ry + rh)
            }
            // If the cursor cannot be found, stay click-through: swallowing every
            // click on the desktop is a far worse failure than not being grabbable.
            None => false,
        };
        let was = self.click_through;
        self.set_click_through(!on_blob)?;
        Ok(was != self.click_through)
    }

    /// Give the mouse back to everyone else on the way out.
    ///
    /// Click-through rather than interactive, for the same reason as macOS: a
    /// desktop-sized invisible window that swallows clicks is the worse state to
    /// leave behind for the moment before the window is destroyed.
    pub fn restore_input_region(&mut self) -> Result<(), String> {
        self.win()?;
        let style = self.ex_style();
        self.set_ex_style(style | WS_EX_TRANSPARENT as isize);
        self.click_through = true;
        Ok(())
    }

    /// There is no region held by the system to read back. Report what the window is
    /// actually doing instead, in the same shape the X11 version returns.
    pub fn read_back_input_region(&self) -> Result<Vec<(i16, i16, u16, u16)>, String> {
        if self.click_through {
            return Ok(Vec::new());
        }
        Ok(self
            .rects
            .iter()
            .map(|(x, y, w, h)| (*x as i16, *y as i16, *w as u16, *h as u16))
            .collect())
    }

    /// Nothing to hint. Win32 has no ICCCM and no window manager that will argue
    /// about where a borderless window goes.
    pub fn set_size_hints(&self) -> Result<(), String> {
        Ok(())
    }

    /// Re-apply the extended styles and the DWM request after the window has been
    /// shown, in case `ShowWindow` reset either.
    pub fn nudge_state(&mut self) -> Result<(), String> {
        self.apply_overlay_properties()
    }

    /// The window covers the desktop exactly, so in its own coordinates it is always
    /// at the origin. See the note on `Win::origin`.
    pub fn window_rect(&self) -> Result<(i32, i32, u32, u32), String> {
        Ok((0, 0, self.size.0, self.size.1))
    }

    /// Nothing to re-assert: no window manager moved us.
    pub fn force_full_screen_geometry(&self) -> Result<(), String> {
        Ok(())
    }

    /// DWM composites every window with an alpha channel, so there is no visual to
    /// get wrong. Reported as 32 to mean "has alpha", which is what the strategy loop
    /// in `main.rs` looks for.
    pub fn depth_of(&self, _handle: u64) -> Result<u8, String> {
        Ok(32)
    }

    /// The platform-specific half of the startup diagnostics.
    pub fn describe(&self, handle: u64) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        out.push(("HWND", format!("{handle:#018x}")));
        out.push((
            "desktop",
            format!(
                "{} x {} at ({}, {})",
                self.size.0, self.size.1, self.origin.0, self.origin.1
            ),
        ));
        // The one that decides whether this is an overlay or a rectangle.
        out.push((
            "DWM alpha",
            match &self.composition {
                Ok(()) => "on (blur-behind, empty region) — the window composites with alpha"
                    .to_string(),
                Err(e) => format!("FAILED  <-- the overlay will be opaque: {e}"),
            },
        ));
        out.push((
            "extended style",
            "TOPMOST | TOOLWINDOW | NOACTIVATE (not LAYERED — see the module docs)".into(),
        ));
        for d in &self.displays {
            out.push((
                "display",
                format!(
                    "{:<12} {:>5} x {:<5} at ({:>5}, {:>5}){}",
                    d.name,
                    d.width,
                    d.height,
                    d.x,
                    d.y,
                    if d.primary { "  [primary]" } else { "" }
                ),
            ));
        }
        out.push((
            "click-through",
            "WS_EX_TRANSPARENT; the cursor is tested against the blob each frame".into(),
        ));
        out
    }
}

/// The pointer's position in SDL's global desktop coordinates.
///
/// `GetCursorPos` is in physical screen pixels, which is the same space
/// `SDL_GetDisplayBounds` reports once the process is per-monitor DPI aware — which
/// `prepare_process_environment` arranges before SDL starts.
fn global_cursor() -> Option<(i32, i32)> {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: `p` is a live local; `GetCursorPos` writes two ints into it and
    // returns zero on failure without touching it.
    let ok = unsafe { GetCursorPos(&mut p) };
    if ok == 0 { None } else { Some((p.x, p.y)) }
}
