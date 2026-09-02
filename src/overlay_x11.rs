//! X11 side of the overlay: EWMH window properties and the XShape input region.
//!
//! Everything here talks to the same X server SDL is using (`DISPLAY`, i.e. XWayland
//! on this box) through `x11rb`, which is pure Rust and needs no C headers.
//!
//! The interesting part is the input region. `ShapeInput` decides which pixels of the
//! window the X server will deliver pointer events to; an empty region means the
//! whole window is click-through and the desktop underneath behaves as if we were
//! not there. `ShapeBounding` would also work as a hit test, but it clips the
//! rendered pixels too, which would hard-edge the antialiased rim — so: input only.


// ---------------------------------------------------------------------------
// The surface every platform's overlay has to provide.
//
// `main.rs` names this module `overlay` whichever platform it is built for, and
// calls only the items below plus the inherent methods on `Overlay`. The macOS
// twin of this file, `overlay_mac.rs`, provides exactly the same set — so the
// frame loop is written once and neither platform is a special case inside it.
// ---------------------------------------------------------------------------

/// What `main.rs` calls the thing that holds the window on the desktop.
pub type Overlay = X11;

/// Anything that has to be true of the process before SDL initialises.
///
/// SDL has no Wayland layer-shell support, so an always-on-top click-through
/// overlay is not reachable from a Wayland-native surface. Force the X11 backend
/// (XWayland) before SDL_Init looks at the environment.
pub fn prepare_process_environment() {
    // SAFETY: called from `run` before SDL or any thread of ours exists, so
    // nothing can be reading the environment concurrently.
    unsafe {
        std::env::set_var("SDL_VIDEODRIVER", "x11");
        // And drop WAYLAND_DISPLAY for our own process only. Forcing SDL's video
        // driver is not enough: EGL picks its *platform* by sniffing the
        // environment, so with WAYLAND_DISPLAY still set it selects the Wayland
        // platform, hands the display off to libnvidia-egl-wayland, and that
        // segfaults inside wl_proxy_create_wrapper because we never made a Wayland
        // connection. Removing it makes EGL choose the X11 platform, matching the
        // X11 window we are actually creating.
        std::env::remove_var("WAYLAND_DISPLAY");
        // Belt and braces for the same problem: SDL 2.26's X11 loader passes
        // platform 0 to eglGetPlatformDisplay, and the vendor loader then guesses.
        // We do not ask for EGL, but if anything ever does, this keeps it on X11.
        std::env::set_var("EGL_PLATFORM", "x11");
    }
}

/// Frame budget while the blob is asleep: ~15 fps.
///
/// The XShape input region means the X server wakes us the instant the pointer
/// touches the blob, so idling this slowly costs nothing in responsiveness.
pub const IDLE_WAIT_MS: u32 = 66;

/// The input region *is* the hit test here: a click that reaches this process
/// already landed on the blob.
pub const REGION_IS_THE_HIT_TEST: bool = true;

/// Getting a transparent window means choosing an X visual by hand and checking
/// what the driver did with it, so the overlay has a list of strategies to try.
pub const NEEDS_VISUAL_STRATEGY: bool = true;

/// The SDL video driver this overlay can actually work with.
pub const REQUIRED_VIDEO_DRIVER: &str = "x11";

pub fn check_video_driver(driver: &str) -> Result<(), String> {
    if driver == REQUIRED_VIDEO_DRIVER {
        return Ok(());
    }
    Err(format!(
        "SDL chose the {driver:?} video driver, but liquidMetal needs \
         {REQUIRED_VIDEO_DRIVER:?} here.\n\
         The overlay relies on X11 features (ARGB visuals, EWMH, XShape) that have \
         no SDL-reachable equivalent on Wayland.\n\
         Check that an X server or XWayland is running on $DISPLAY."
    ))
}

/// Pull the native window handle out of SDL: on X11, the window id.
///
/// Route (a) from the two available: `SDL_GetWindowWMInfo` with a version-stamped
/// `SDL_SysWMinfo`. Chosen over the `raw-window-handle` route because that one
/// panics internally when the query fails, and window setup is exactly where a
/// panic is least useful.
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
                 Without the X window id the overlay cannot set its EWMH properties \
                 or its click-through input region.",
                sdl2::get_error()
            ));
        }
        if info.subsystem != sdl2::sys::SDL_SYSWM_TYPE::SDL_SYSWM_X11 {
            return Err(format!(
                "the SDL window is not an X11 window (SDL_SYSWM_TYPE = {}).\n\
                 liquidMetal must run as an X11 client; set DISPLAY and make sure \
                 XWayland is available.",
                info.subsystem as u32
            ));
        }
        Ok(info.info.x11.window as u64)
    }
}

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ClipOrdering, ConnectionExt as _, EventMask, PropMode,
    Rectangle, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// `_NET_WM_STATE` client-message action: add the listed states.
const NET_WM_STATE_ADD: u32 = 1;
/// `_NET_WM_DESKTOP` value meaning "show on every desktop".
const ALL_DESKTOPS: u32 = 0xFFFF_FFFF;

macro_rules! atoms {
    ($($field:ident => $name:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy)]
        struct Atoms { $( $field: xproto::Atom, )* }

        impl Atoms {
            fn intern(conn: &RustConnection) -> Result<Atoms, String> {
                // Fire every InternAtom first, then collect, so this costs one
                // round-trip rather than one per atom.
                $( let $field = conn
                    .intern_atom(false, $name)
                    .map_err(|e| format!("InternAtom({}) could not be sent: {e}",
                                         String::from_utf8_lossy($name)))?; )*
                Ok(Atoms {
                    $( $field: $field
                        .reply()
                        .map_err(|e| format!("InternAtom({}) failed: {e}",
                                             String::from_utf8_lossy($name)))?
                        .atom, )*
                })
            }
        }
    };
}

atoms! {
    net_wm_state            => b"_NET_WM_STATE",
    state_above             => b"_NET_WM_STATE_ABOVE",
    state_sticky            => b"_NET_WM_STATE_STICKY",
    state_skip_taskbar      => b"_NET_WM_STATE_SKIP_TASKBAR",
    state_skip_pager        => b"_NET_WM_STATE_SKIP_PAGER",
    net_wm_window_type      => b"_NET_WM_WINDOW_TYPE",
    // Deliberately UTILITY and not DOCK: KWin reserves screen-edge struts for DOCK
    // windows, which would shove every other window aside.
    window_type_utility     => b"_NET_WM_WINDOW_TYPE_UTILITY",
    net_wm_desktop          => b"_NET_WM_DESKTOP",
    net_wm_user_time        => b"_NET_WM_USER_TIME",
    // KWin sets this itself when a window maps without taking focus. Harmless, but
    // it makes panels highlight us, so it gets cleared explicitly after mapping.
    state_demands_attention => b"_NET_WM_STATE_DEMANDS_ATTENTION",
}

/// `_NET_WM_STATE` client-message action: remove the listed states.
const NET_WM_STATE_REMOVE: u32 = 0;

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub name: String,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
}

pub struct X11 {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    /// The SDL window's XID, once known.
    win: Option<Window>,
    /// Last input region actually uploaded, so we can skip identical round-trips.
    last_rects: Option<Vec<Rectangle>>,
    /// Full X virtual screen: the union of every monitor, which is what the overlay
    /// window is sized to so the blob can be dragged between displays.
    pub virtual_w: u16,
    pub virtual_h: u16,
    pub shape_version: (u16, u16),
    pub monitors: Vec<MonitorInfo>,
    /// A depth-32 TrueColor visual, if the screen has one. See `find_argb_visual`.
    pub argb_visual: Option<u32>,
}

impl X11 {
    /// Connect to `$DISPLAY` and gather everything the caller needs *before* the
    /// window exists — notably the virtual screen size.
    /// The whole desktop, in logical pixels: how big the overlay window has to be.
    pub fn desktop_size(&self) -> (u32, u32) {
        (self.virtual_w as u32, self.virtual_h as u32)
    }

    /// The X root window is the origin, always.
    pub fn desktop_origin(&self) -> (i32, i32) {
        (0, 0)
    }

    /// Nothing to do once GL is up: the window's visual was settled before it was
    /// created, and that is the whole of transparency on X11.
    pub fn on_gl_context_ready(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Nothing to do here: the X server told us the virtual screen at `connect`
    /// time, before SDL existed. macOS cannot answer until SDL's video subsystem is
    /// up, which is why this step exists at all.
    pub fn probe_desktop(&mut self, _video: &sdl2::VideoSubsystem) -> Result<(), String> {
        Ok(())
    }

    pub fn connect() -> Result<X11, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| {
            format!(
                "could not connect to the X server: {e}\n\
                 liquidMetal runs as an X11 client (XWayland is fine). Check that \
                 DISPLAY is set and that an X server is reachable there."
            )
        })?;

        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_num)
            .ok_or_else(|| format!("X screen {screen_num} does not exist"))?;
        let root = screen.root;
        let virtual_w = screen.width_in_pixels;
        let virtual_h = screen.height_in_pixels;

        let shape_version = shape::query_version(&conn)
            .map_err(|e| format!("the XShape query could not be sent: {e}"))?
            .reply()
            .map_err(|e| {
                format!(
                    "the XShape extension is unavailable: {e}\n\
                     Without it the overlay cannot be made click-through. \
                     Run with --windowed to use an ordinary window instead."
                )
            })?;

        let monitors = query_monitors(&conn, root).unwrap_or_default();
        let argb_visual = find_argb_visual(screen);
        let atoms = Atoms::intern(&conn)?;

        Ok(X11 {
            conn,
            root,
            atoms,
            win: None,
            last_rects: None,
            virtual_w,
            virtual_h,
            shape_version: (shape_version.major_version, shape_version.minor_version),
            monitors,
            argb_visual,
        })
    }

    /// Adopt the XID SDL handed us. X window IDs are 32-bit; the SDL/Xlib type is a
    /// `c_ulong`, so this is where that gets checked rather than blindly truncated.
    pub fn set_window(&mut self, xid: u64) -> Result<(), String> {
        let w: Window = xid
            .try_into()
            .map_err(|_| format!("SDL reported an X window id that is not 32-bit: {xid:#x}"))?;
        if w == 0 {
            return Err("SDL reported a null X window id".to_string());
        }
        self.win = Some(w);
        Ok(())
    }

    fn win(&self) -> Result<Window, String> {
        self.win
            .ok_or_else(|| "no X window has been attached yet".to_string())
    }

    /// Set every EWMH property the overlay depends on.
    ///
    /// Call this while the window is still unmapped: a window manager reads
    /// `_NET_WM_STATE` and `_NET_WM_WINDOW_TYPE` at map time. `nudge_state` below
    /// re-asserts the state afterwards for the case where it did not.
    pub fn apply_overlay_properties(&self) -> Result<(), String> {
        let win = self.win()?;
        let a = &self.atoms;

        self.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                a.net_wm_window_type,
                AtomEnum::ATOM,
                &[a.window_type_utility],
            )
            .map_err(|e| format!("could not set _NET_WM_WINDOW_TYPE: {e}"))?;

        self.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                a.net_wm_state,
                AtomEnum::ATOM,
                &[
                    a.state_above,
                    a.state_sticky,
                    a.state_skip_taskbar,
                    a.state_skip_pager,
                ],
            )
            .map_err(|e| format!("could not set _NET_WM_STATE: {e}"))?;

        self.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                a.net_wm_desktop,
                AtomEnum::CARDINAL,
                &[ALL_DESKTOPS],
            )
            .map_err(|e| format!("could not set _NET_WM_DESKTOP: {e}"))?;

        // A zero user-time asks the window manager not to give us focus when we map.
        // Cheap, and it keeps the overlay from stealing your keyboard on startup.
        self.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                a.net_wm_user_time,
                AtomEnum::CARDINAL,
                &[0],
            )
            .map_err(|e| format!("could not set _NET_WM_USER_TIME: {e}"))?;

        self.conn
            .flush()
            .map_err(|e| format!("could not flush the X connection: {e}"))
    }

    /// Re-assert the window states via root client messages, which is the only route
    /// a window manager is obliged to honour once the window is already mapped.
    pub fn nudge_state(&self) -> Result<(), String> {
        let win = self.win()?;
        let a = &self.atoms;
        // Two states per message is the EWMH maximum.
        for (action, pair) in [
            (NET_WM_STATE_ADD, [a.state_above, a.state_sticky]),
            (NET_WM_STATE_ADD, [a.state_skip_taskbar, a.state_skip_pager]),
            (NET_WM_STATE_REMOVE, [a.state_demands_attention, 0]),
        ] {
            let ev = ClientMessageEvent::new(
                32,
                win,
                a.net_wm_state,
                [action, pair[0], pair[1], 1, 0],
            );
            self.conn
                .send_event(
                    false,
                    self.root,
                    EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                    ev,
                )
                .map_err(|e| format!("could not send the _NET_WM_STATE message: {e}"))?;
        }
        self.conn
            .flush()
            .map_err(|e| format!("could not flush the X connection: {e}"))
    }

    /// Make the entire window click-through.
    pub fn set_input_region_empty(&mut self) -> Result<(), String> {
        self.upload(Vec::new(), true)
    }

    /// Give the window back a full-size input region. Used on shutdown so nothing
    /// odd is left behind if the window outlives us for a moment.
    pub fn restore_input_region(&mut self) -> Result<(), String> {
        let r = vec![Rectangle {
            x: 0,
            y: 0,
            width: self.virtual_w,
            height: self.virtual_h,
        }];
        self.upload(r, true)
    }

    /// Set the input region to `rects` (top-down window-relative pixels).
    ///
    /// Returns `true` if it actually reached the server. Identical regions are
    /// dropped: this is a round-trip, and the blob's rects are quantised to a coarse
    /// grid precisely so that a slow drag does not produce one per frame.
    pub fn set_input_rects(&mut self, rects: &[(i32, i32, i32, i32)]) -> Result<bool, String> {
        let converted: Vec<Rectangle> = rects
            .iter()
            .filter_map(|&(x, y, w, h)| {
                if w <= 0 || h <= 0 {
                    return None;
                }
                Some(Rectangle {
                    x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    width: w.min(u16::MAX as i32) as u16,
                    height: h.min(u16::MAX as i32) as u16,
                })
            })
            .collect();

        if self
            .last_rects
            .as_deref()
            .is_some_and(|prev| rects_eq(prev, &converted))
        {
            return Ok(false);
        }
        self.upload(converted, false)?;
        Ok(true)
    }

    /// The depth X actually gave the window. Anything other than 32 in overlay mode
    /// means there is no alpha channel and the window will composite as opaque.
    /// The platform-specific half of the startup diagnostics, as label/value pairs.
    ///
    /// Everything here is *read back from the X server* rather than assumed: what
    /// SDL asked for and what it got differ in exactly the ways that make an
    /// overlay fail silently, so the log states the truth and says so loudly when
    /// the truth is wrong.
    pub fn describe(&self, xid: u64) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        out.push(("window XID", format!("{xid:#010x} ({xid})")));
        match self.window_rect() {
            Ok((wx, wy, ww, wh)) => out.push((
                "window on screen",
                format!(
                    "{ww} x {wh} at ({wx}, {wy}){}",
                    if (wx, wy) == (0, 0)
                        && (ww, wh) == (self.virtual_w as u32, self.virtual_h as u32)
                    {
                        "  [matches the virtual screen]"
                    } else {
                        "  <-- NOT the full virtual screen"
                    }
                ),
            )),
            Err(e) => out.push(("window on screen", format!("unknown ({e})"))),
        }
        // The authoritative transparency check: GL alpha bits are not enough, the X
        // window itself has to be depth 32.
        match self.window_depth() {
            Ok(32) => out.push((
                "X window depth",
                format!(
                    "32  <-- ARGB visual {}, alpha channel present",
                    self.argb_visual
                        .map(|v| format!("{v:#x}"))
                        .unwrap_or_else(|| "(SDL's choice)".into())
                ),
            )),
            Ok(d) => out.push((
                "X window depth",
                format!("{d}  <-- NO ALPHA CHANNEL: the overlay will be opaque"),
            )),
            Err(e) => out.push(("X window depth", format!("unknown ({e})"))),
        }
        out.push((
            "XShape version",
            format!("{}.{}", self.shape_version.0, self.shape_version.1),
        ));
        out.push((
            "X virtual screen",
            format!("{} x {} at (0, 0)", self.virtual_w, self.virtual_h),
        ));
        if self.monitors.is_empty() {
            out.push(("monitors", "(RandR 1.5 GetMonitors unavailable)".into()));
        } else {
            for m in &self.monitors {
                out.push((
                    "monitor",
                    format!(
                        "{:<12} {:>5} x {:<5} at ({:>5}, {:>5}){}",
                        m.name,
                        m.width,
                        m.height,
                        m.x,
                        m.y,
                        if m.primary { "  [primary]" } else { "" }
                    ),
                ));
            }
        }
        out.push(("input region", "empty (whole window click-through)".into()));
        out
    }

    pub fn window_depth(&self) -> Result<u8, String> {
        self.depth_of(self.win()? as u64)
    }

    /// Depth of an arbitrary window id, usable before `set_window`.
    pub fn depth_of(&self, xid: u64) -> Result<u8, String> {
        let win: Window = xid
            .try_into()
            .map_err(|_| format!("not a 32-bit X window id: {xid:#x}"))?;
        Ok(self
            .conn
            .get_geometry(win)
            .map_err(|e| format!("GetGeometry could not be sent: {e}"))?
            .reply()
            .map_err(|e| format!("GetGeometry failed: {e}"))?
            .depth)
    }

    /// The window's real rectangle in root (screen) coordinates.
    ///
    /// Not the same as what we asked for: a window manager is free to move or resize
    /// us, and if it does, the overlay's idea of the screen no longer matches the
    /// screen — the blob would bounce off walls that are not where it is drawn.
    pub fn window_rect(&self) -> Result<(i32, i32, u32, u32), String> {
        let win = self.win()?;
        let geo = self
            .conn
            .get_geometry(win)
            .map_err(|e| format!("GetGeometry could not be sent: {e}"))?
            .reply()
            .map_err(|e| format!("GetGeometry failed: {e}"))?;
        // GetGeometry is relative to the parent, which under a reparenting window
        // manager is a frame, not the root. Translate to be sure.
        let abs = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)
            .map_err(|e| format!("TranslateCoordinates could not be sent: {e}"))?
            .reply()
            .map_err(|e| format!("TranslateCoordinates failed: {e}"))?;
        Ok((
            abs.dst_x as i32,
            abs.dst_y as i32,
            geo.width as u32,
            geo.height as u32,
        ))
    }

    /// Tell the window manager that our position and size are deliberate.
    ///
    /// Without `USPosition` in WM_NORMAL_HINTS, KWin runs its own placement policy
    /// and drops a borderless utility window at the primary monitor's origin — on a
    /// dual-head desktop that is x=1920, which pushes the right half of a
    /// full-virtual-screen window clean off the edge of the display. The `US*` flags
    /// are the ICCCM way of saying "the user asked for exactly this", which window
    /// managers honour where they would override a mere program request.
    ///
    /// Must be set before the window is mapped.
    pub fn set_size_hints(&self) -> Result<(), String> {
        let win = self.win()?;
        let w = self.virtual_w as u32;
        let h = self.virtual_h as u32;

        const US_POSITION: u32 = 1;
        const US_SIZE: u32 = 1 << 1;
        const P_POSITION: u32 = 1 << 2;
        const P_SIZE: u32 = 1 << 3;
        const P_MIN_SIZE: u32 = 1 << 4;
        const P_MAX_SIZE: u32 = 1 << 5;
        const P_WIN_GRAVITY: u32 = 1 << 9;
        const STATIC_GRAVITY: u32 = 10;

        // ICCCM WM_SIZE_HINTS: 18 words. Fields 1..4 are the obsolete x/y/w/h, kept
        // populated because some window managers still read them.
        let hints: [u32; 18] = [
            US_POSITION | US_SIZE | P_POSITION | P_SIZE | P_MIN_SIZE | P_MAX_SIZE
                | P_WIN_GRAVITY,
            0,
            0, // obsolete x, y
            w,
            h, // obsolete width, height
            w,
            h, // min size
            w,
            h, // max size
            0,
            0, // resize increments
            0,
            0,
            0,
            0, // aspect ratios
            0,
            0,              // base size
            STATIC_GRAVITY, // no offsetting for a frame we do not have
        ];

        self.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                AtomEnum::WM_NORMAL_HINTS,
                AtomEnum::WM_SIZE_HINTS,
                &hints,
            )
            .map_err(|e| format!("could not set WM_NORMAL_HINTS: {e}"))?;
        self.conn
            .flush()
            .map_err(|e| format!("could not flush the X connection: {e}"))
    }

    /// Put the window back at the origin at full virtual-screen size.
    ///
    /// KWin will happily constrain a freshly mapped window to one monitor's work
    /// area; this asks for the whole virtual screen back.
    /// Nothing to re-assert per frame. The macOS twin rebuilds its GL surface here,
    /// because on that platform transparency does not stick until the window has been
    /// presented; X11 settles it once, when the visual is chosen.
    pub fn on_frame(&mut self) {}

    pub fn force_full_screen_geometry(&self) -> Result<(), String> {
        let win = self.win()?;
        self.conn
            .configure_window(
                win,
                &xproto::ConfigureWindowAux::new()
                    .x(0)
                    .y(0)
                    .width(self.virtual_w as u32)
                    .height(self.virtual_h as u32),
            )
            .map_err(|e| format!("could not reposition the window: {e}"))?;
        self.conn
            .flush()
            .map_err(|e| format!("could not flush the X connection: {e}"))
    }

    /// Ask the server what the input region actually is right now.
    ///
    /// This is the honest check that the region reached X and was accepted, rather
    /// than trusting that a request we sent had the effect we intended.
    pub fn read_back_input_region(&self) -> Result<Vec<(i16, i16, u16, u16)>, String> {
        let win = self.win()?;
        let reply = shape::get_rectangles(&self.conn, win, SK::INPUT)
            .map_err(|e| format!("ShapeGetRectangles could not be sent: {e}"))?
            .reply()
            .map_err(|e| format!("ShapeGetRectangles failed: {e}"))?;
        Ok(reply
            .rectangles
            .into_iter()
            .map(|r| (r.x, r.y, r.width, r.height))
            .collect())
    }

    fn upload(&mut self, rects: Vec<Rectangle>, force: bool) -> Result<(), String> {
        let win = self.win()?;
        if !force
            && self
                .last_rects
                .as_deref()
                .is_some_and(|prev| rects_eq(prev, &rects))
        {
            return Ok(());
        }
        // SO::SET replaces the region wholesale, so an empty slice really does mean
        // "no input anywhere", which is the click-through case.
        self.conn
            .shape_rectangles(
                SO::SET,
                SK::INPUT,
                ClipOrdering::UNSORTED,
                win,
                0,
                0,
                &rects,
            )
            .map_err(|e| format!("could not set the XShape input region: {e}"))?;
        self.conn
            .flush()
            .map_err(|e| format!("could not flush the X connection: {e}"))?;
        self.last_rects = Some(rects);
        Ok(())
    }
}

/// `xproto::Rectangle` is a foreign type with no `PartialEq`, so the "has the region
/// actually changed?" test is spelled out here. This is what keeps a slow drag from
/// producing one X round-trip per frame.
fn rects_eq(a: &[Rectangle], b: &[Rectangle]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(p, q)| {
            p.x == q.x && p.y == q.y && p.width == q.width && p.height == q.height
        })
}

/// Find a 32-bit TrueColor visual — one with a real alpha channel.
///
/// This is the whole ballgame for a transparent overlay, and it is *not* what
/// `SDL_GL_SetAttribute(SDL_GL_ALPHA_SIZE, 8)` gets you on its own. GLX reports
/// alpha bits for the *GL colour buffer*, which depth-24 visuals on this driver
/// happily claim (there are 640 such fbconfigs here versus 32 real 32-bit ones).
/// SDL then takes `glXChooseFBConfig(...)[0]`, lands on a depth-24 visual, and you
/// get a window with no alpha channel: `SDL_GL_ALPHA_SIZE` reads back as 8 and the
/// screen goes solid black behind the blob.
///
/// So we pick the visual ourselves and hand it to SDL via
/// `SDL_VIDEO_X11_WINDOW_VISUALID`, which SDL honours for window creation — and
/// since `X11_GL_CreateContext` derives the context from the window's own visual,
/// the two stay consistent.
fn find_argb_visual(screen: &xproto::Screen) -> Option<u32> {
    screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| d.visuals.iter())
        .find(|v| {
            v.class == xproto::VisualClass::TRUE_COLOR
                // A depth-32 TrueColor visual's RGB masks cover 24 bits; the
                // remaining 8 are the alpha channel.
                && v.red_mask | v.green_mask | v.blue_mask == 0x00ff_ffff
        })
        .map(|v| v.visual_id)
}

fn query_monitors(conn: &RustConnection, root: Window) -> Option<Vec<MonitorInfo>> {
    // RandR 1.5 is what provides GetMonitors; older servers simply report nothing
    // here and the diagnostic block says so.
    conn.randr_query_version(1, 5).ok()?.reply().ok()?;
    let reply = conn.randr_get_monitors(root, true).ok()?.reply().ok()?;
    let mut out = Vec::new();
    for m in reply.monitors {
        let name = conn
            .get_atom_name(m.name)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .unwrap_or_else(|| format!("atom {}", m.name));
        out.push(MonitorInfo {
            name,
            x: m.x,
            y: m.y,
            width: m.width,
            height: m.height,
            primary: m.primary,
        });
    }
    Some(out)
}
