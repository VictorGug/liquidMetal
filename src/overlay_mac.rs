//! macOS side of the overlay: Cocoa window properties and click-through.
//!
//! The twin of `overlay_x11.rs`. It provides the same items to `main.rs`, so the
//! frame loop does not know which platform it is on. What the two do underneath
//! could hardly be less alike.
//!
//! # The three things that make a window an overlay, and how macOS spells them
//!
//! | | X11 | macOS |
//! | --- | --- | --- |
//! | transparent | pick a depth-32 ARGB visual by hand | `setOpaque:NO` + a clear background **+ `NSOpenGLCPSurfaceOpacity` = 0** |
//! | always on top | `_NET_WM_STATE_ABOVE` | `setLevel:` |
//! | click-through except on the blob | XShape `ShapeInput` region | there is no such thing — see below |
//!
//! # There is no input region on macOS
//!
//! This is the one real architectural difference. X11 hands the server a set of
//! rectangles and it decides, per pixel, whether a click belongs to us; the region
//! *is* the hit test, and the app only ever hears about clicks that landed on the
//! blob. Cocoa has no equivalent: `setIgnoresMouseEvents:` is all-or-nothing for the
//! whole window.
//!
//! So the region is emulated. Every frame, the global cursor position is compared
//! against the same rectangle cover `physics::hit_rects` produces for X11, and the
//! window is switched between "swallows clicks" and "invisible to the mouse"
//! accordingly. `set_input_rects` therefore has the same signature and the same
//! meaning on both platforms; only the mechanism differs.
//!
//! Two consequences worth knowing:
//!
//! - The decision is one frame stale. A cursor moving faster than the frame rate can
//!   cross onto the blob and click before the window has stopped ignoring the mouse.
//!   That is why macOS also runs the local hit test (`App::check_hit`), and why the
//!   idle frame budget here is shorter than on X11: while the window is ignoring the
//!   mouse it receives no events at all, so nothing can wake it — it has to look.
//! - While the window is *not* ignoring the mouse it swallows clicks over its whole
//!   surface, which is the entire desktop. The window is therefore only made
//!   clickable when the pointer is genuinely on the blob, and put back immediately
//!   afterwards.

use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

/// `CGColorRef` is a pointer to an opaque Core Graphics struct, not an object.
///
/// It matters because `-[CALayer setBackgroundColor:]` takes one, while its NSWindow
/// namesake takes an `NSColor *`. Passing an object pointer to the layer is wrong in
/// a way release builds run happily and a debug build refuses outright: objc2 checks
/// the selector's type encoding, and `@` where `^{CGColor=}` is expected is a panic.
#[repr(C)]
struct CGColor {
    _opaque: [u8; 0],
}

// SAFETY: the encodings are the ones the Objective-C runtime reports for CGColorRef.
unsafe impl Encode for CGColor {
    const ENCODING: Encoding = Encoding::Struct("CGColor", &[]);
}
unsafe impl RefEncode for CGColor {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

// ---------------------------------------------------------------------------
// TUNABLES
// ---------------------------------------------------------------------------

/// `NSStatusWindowLevel`. High enough to float above ordinary windows *and* the
/// menu bar, which is what makes the overlay cover the whole screen rather than
/// stopping 25 points down. `NSFloatingWindowLevel` (3) is the politer choice if
/// you would rather the menu bar won.
const WINDOW_LEVEL: i64 = 25;

/// How opaque the window claims to be. Anything below 1.0 puts the window on the
/// WindowServer's blended path, which is the only path that consults the alpha the
/// shader writes; at exactly 1.0 the overlay is a black rectangle however
/// transparent everything underneath is set to be. See `set_window_transparency`.
const WINDOW_ALPHA: f64 = 0.99;

/// When to re-assert transparency after the window is shown, in milliseconds.
///
/// Doing it once is not enough and the reason is timing: the GL surface is not
/// rebuilt against the new window state until the window has actually been presented
/// a few times, so the whole sequence applied before the first frame is applied to
/// nothing and the overlay comes up black. Re-asserting on a short schedule costs a
/// handful of Objective-C messages and covers whatever the real threshold is on a
/// given machine. The last one is late enough to survive a slow first frame.
const REASSERT_MS: &[u64] = &[200, 500, 1000, 2000, 4000];

/// `NSWindowCollectionBehaviorCanJoinAllSpaces | Stationary | FullScreenAuxiliary`.
/// Follow the user between Spaces instead of belonging to the one it was born on,
/// do not slide around during Exposé, and be allowed to sit over a full-screen app.
const COLLECTION_BEHAVIOR: u64 = (1 << 0) | (1 << 4) | (1 << 8);

/// `NSOpenGLCPSurfaceOpacity`. Setting this parameter to 0 is what actually makes
/// the GL surface composite with alpha.
///
/// This is the macOS counterpart of the ARGB-visual trap on X11, and it fails the
/// same silent way: `SDL_GL_ALPHA_SIZE` reads back as 8, the window is `setOpaque:NO`,
/// everything looks correct — and the blob sits on a solid black rectangle the size
/// of your desktop, because the GL surface underneath the transparent window is
/// still opaque.
const NSOPENGL_CP_SURFACE_OPACITY: i64 = 236;

// ---------------------------------------------------------------------------
// The surface every platform's overlay has to provide.
// ---------------------------------------------------------------------------

/// What `main.rs` calls the thing that holds the window on the desktop.
pub type Overlay = Mac;

/// Nothing to arrange before SDL starts. The X11 build has a long story here about
/// EGL platform sniffing; Cocoa has no equivalent problem.
pub fn prepare_process_environment() {}

/// Frame budget while the blob is asleep: ~33 fps.
///
/// Shorter than the X11 budget, and for a real reason rather than taste. While the
/// window is ignoring the mouse it receives no events whatsoever, so nothing can
/// wake the loop when the pointer arrives over the blob — the only way to notice is
/// to look. This is how often it looks. Each of those wake-ups is one
/// `SDL_GetGlobalMouseState` and a handful of rectangle comparisons.
pub const IDLE_WAIT_MS: u32 = 30;

/// There is no input region, so the local hit test is not redundant here.
pub const REGION_IS_THE_HIT_TEST: bool = false;

/// Cocoa composites every window; there is no visual to choose and nothing to
/// retry if it goes wrong.
pub const NEEDS_VISUAL_STRATEGY: bool = false;

pub const REQUIRED_VIDEO_DRIVER: &str = "cocoa";

pub fn check_video_driver(driver: &str) -> Result<(), String> {
    if driver == REQUIRED_VIDEO_DRIVER {
        return Ok(());
    }
    Err(format!(
        "SDL chose the {driver:?} video driver, but the macOS overlay needs \
         {REQUIRED_VIDEO_DRIVER:?}.\n\
         The overlay sends Objective-C messages to the NSWindow behind the SDL \
         window, which only exists on the Cocoa backend."
    ))
}

/// Pull the native window handle out of SDL: on macOS, the `NSWindow *`.
///
/// Read out of the union's byte array rather than a `cocoa` field, because there is
/// no such field to read. `sdl2-sys` ships **one** pre-generated `sdl_bindings.rs`
/// for every platform, generated on Linux, so the `SDL_SysWMinfo` union it declares
/// has `x11`, `wl` and `dummy` and nothing else — on macOS as much as here.
///
/// That is fine, because the union is deliberately ABI-stable: `SDL_syswm.h` pads it
/// with `Uint8 dummy[64]` precisely so its size does not vary, and the macOS arm is
/// `struct { NSWindow *window; }` — one pointer, at offset zero. Reading the first
/// eight bytes of `dummy` is therefore exactly reading `info.info.cocoa.window`, and
/// reading the `dummy` arm of a union is always valid since `u8` has no invalid bit
/// patterns.
///
/// The `raw-window-handle` route does not work here: `sdl2` builds its
/// `AppKitWindowHandle` from `SDL_MetalView`, which a window created for OpenGL does
/// not have.
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
                 Without the NSWindow the overlay cannot make itself transparent, \
                 float above other windows, or let clicks through.",
                sdl2::get_error()
            ));
        }
        if info.subsystem != sdl2::sys::SDL_SYSWM_TYPE::SDL_SYSWM_COCOA {
            return Err(format!(
                "the SDL window is not a Cocoa window (SDL_SYSWM_TYPE = {}).\n\
                 The macOS overlay needs the NSWindow behind the SDL window.",
                info.subsystem as u32
            ));
        }
        let mut ptr = [0u8; 8];
        ptr.copy_from_slice(&info.info.dummy[..8]);
        let handle = u64::from_ne_bytes(ptr);
        if handle == 0 {
            return Err("SDL reported a Cocoa window with a null NSWindow".into());
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

pub struct Mac {
    /// `NSWindow *`. Null until `set_window`.
    ns_window: *mut AnyObject,
    /// The union of every display, in SDL's global desktop coordinates.
    ///
    /// The origin can be negative when a display sits left of or above the primary
    /// one. Everything `main.rs` sees is relative to this rectangle, which is why
    /// `window_rect` reports an origin of (0, 0): on macOS the window covers the
    /// desktop exactly, so window-relative *is* desktop-relative, and the
    /// window-manager argument that `visible_bounds` exists to survive on X11 simply
    /// does not happen here.
    origin: (i32, i32),
    size: (u32, u32),
    displays: Vec<DisplayInfo>,
    /// Whether the window is currently ignoring the mouse.
    click_through: bool,
    /// Last rectangle cover handed to `set_input_rects`, kept so the cursor can be
    /// re-tested every frame without the caller having to resend it.
    rects: Vec<(i32, i32, i32, i32)>,
    /// Set once the surface opacity has been applied, so it is only done once.
    surface_made_transparent: bool,
    /// When the window was shown, and which of the re-assert deadlines are left.
    /// See `on_frame` for why the overlay cannot simply be made transparent once.
    shown_at: Option<std::time::Instant>,
    pending_reasserts: &'static [u64],
    /// There is no visual to choose on macOS; Cocoa composites every window. Present
    /// only because `main.rs` reads it on both platforms.
    pub argb_visual: Option<u32>,
}

impl Mac {
    /// Nothing to connect to. The displays cannot be enumerated until SDL's video
    /// subsystem is up, which is what `probe_desktop` is for.
    pub fn connect() -> Result<Mac, String> {
        Ok(Mac {
            ns_window: std::ptr::null_mut(),
            origin: (0, 0),
            size: (0, 0),
            displays: Vec::new(),
            click_through: true,
            rects: Vec::new(),
            surface_made_transparent: false,
            shown_at: None,
            pending_reasserts: REASSERT_MS,
            argb_visual: None,
        })
    }

    /// The union of every attached display.
    ///
    /// Asked of SDL rather than of `NSScreen` on purpose: SDL reports display bounds
    /// in the same top-left-origin coordinate space it will later place the window
    /// in, so there is no chance of getting Cocoa's bottom-left origin flip wrong in
    /// one place and right in another.
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
                // SDL orders displays with the primary first.
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

    /// Where the overlay window belongs, in SDL's global coordinates.
    pub fn desktop_origin(&self) -> (i32, i32) {
        self.origin
    }

    pub fn set_window(&mut self, handle: u64) -> Result<(), String> {
        if handle == 0 {
            return Err("the NSWindow handle is null".into());
        }
        self.ns_window = handle as *mut AnyObject;
        Ok(())
    }

    fn win(&self) -> Result<*mut AnyObject, String> {
        if self.ns_window.is_null() {
            return Err("the overlay has no NSWindow yet".into());
        }
        Ok(self.ns_window)
    }

    /// Make the window transparent, floating, and present on every Space.
    pub fn apply_overlay_properties(&self) -> Result<(), String> {
        let w = self.win()?;
        autoreleasepool(|_| {
            // SAFETY: `w` is a live NSWindow for as long as the SDL window is, and
            // every selector below is a documented AppKit method on NSWindow with
            // the argument types given here.
            unsafe {
                self.set_window_transparency();
                let _: () = msg_send![w, setLevel: WINDOW_LEVEL];
                let _: () = msg_send![w, setCollectionBehavior: COLLECTION_BEHAVIOR];
                // Nothing to drag, nothing to drop on us.
                let _: () = msg_send![w, setHasShadow: false];
                // A borderless window is not in the window cycle anyway, but say so.
                let _: () = msg_send![w, setExcludedFromWindowsMenu: true];
            }
        });
        Ok(())
    }

    /// The window half of being transparent: non-opaque, no background, and not
    /// quite fully opaque.
    ///
    /// The `alphaValue` is the surprising one. A window at alpha 1.0 is handed to the
    /// WindowServer as opaque and its per-pixel alpha is never consulted, however
    /// non-opaque the window and its surface claim to be. Anything below 1.0 puts it
    /// on the blended path, where the alpha the shader writes is finally what gets
    /// composited. 0.99 is the smallest lie that does it: the blob is drawn at 99%
    /// opacity, which is not visible, and the desktop behind it is.
    ///
    /// SAFETY: caller holds a live NSWindow in `self.ns_window`; every selector is a
    /// documented NSWindow method with the argument types given here.
    unsafe fn set_window_transparency(&self) {
        let w = self.ns_window;
        if w.is_null() {
            return;
        }
        unsafe {
            let _: () = msg_send![w, setOpaque: false];
            let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
            let _: () = msg_send![w, setBackgroundColor: clear];
            let _: () = msg_send![w, setAlphaValue: WINDOW_ALPHA];
        }
    }

    /// Turn off the GL surface's opacity, so what the shader writes as alpha is what
    /// the compositor blends with.
    ///
    /// Separate from `apply_overlay_properties` because it needs a different thing
    /// at a different moment: the current GL context rather than the NSWindow, and
    /// so it runs earlier in start-up, as soon as the context is made current and
    /// before the window has been given any of its Cocoa properties. The two do not
    /// interact — one is a window property, the other a context parameter.
    ///
    /// Without this the window is transparent and the surface inside it is not, and
    /// the result is a black rectangle the size of the desktop.
    pub fn on_gl_context_ready(&mut self) -> Result<(), String> {
        // SAFETY: `SDL_GL_GetCurrentContext` returns the NSOpenGLContext SDL created
        // for this window, or null. `setValues:forParameter:` takes a pointer to as
        // many GLints as the parameter needs; `NSOpenGLCPSurfaceOpacity` needs one.
        unsafe {
            let ctx = sdl2::sys::SDL_GL_GetCurrentContext();
            if ctx.is_null() {
                return Err("there is no current GL context to make transparent".into());
            }
            let ctx = ctx as *mut AnyObject;

            // The window first. The surface takes its opacity from the window it is
            // attached to at the moment it is built, so a surface built against the
            // opaque window SDL created stays opaque no matter what is set afterwards.
            self.set_window_transparency();

            // Then the view, which has to be explicitly layer-backed. Left implicit,
            // AppKit gives the GL view a backing layer it composites on the opaque
            // path; asking for the layer by name is what moves it onto the path where
            // per-pixel alpha is honoured at all.
            let view: *mut AnyObject = msg_send![ctx, view];
            if !view.is_null() {
                let _: () = msg_send![view, setWantsLayer: true];
                let layer: *mut AnyObject = msg_send![view, layer];
                if !layer.is_null() {
                    let _: () = msg_send![layer, setOpaque: false];
                    let no_colour: *mut CGColor = std::ptr::null_mut();
                    let _: () = msg_send![layer, setBackgroundColor: no_colour];
                }
            }

            // And only now the parameter, with the surface torn down around it so it
            // is rebuilt while the value is already set. Setting it on a live surface
            // is what fails silently: it reads back as 0 on a screen that is still
            // black, which is the whole trap this function exists to avoid.
            let opacity: i32 = 0;
            let nil: *mut AnyObject = std::ptr::null_mut();
            let _: () = msg_send![ctx, setView: nil];
            let _: () = msg_send![
                ctx,
                setValues: &opacity as *const i32,
                forParameter: NSOPENGL_CP_SURFACE_OPACITY,
            ];
            if !view.is_null() {
                let _: () = msg_send![ctx, setView: view];
            }
            let _: () = msg_send![ctx, makeCurrentContext];
            let _: () = msg_send![ctx, update];
        }
        self.surface_made_transparent = true;
        Ok(())
    }

    /// Re-assert transparency over the first few frames after the window is shown.
    ///
    /// The single most surprising thing about the macOS overlay. Every part of making
    /// the surface transparent can be applied, read back as correct, and still leave a
    /// black rectangle, because the surface is only rebuilt against the window's
    /// current state once the window has been presented — which has not happened yet
    /// at the point where all the obvious hooks run. So the sequence is repeated on
    /// the schedule in `REASSERT_MS` and then left alone.
    pub fn on_frame(&mut self) {
        let Some(shown) = self.shown_at else { return };
        let elapsed = shown.elapsed().as_millis() as u64;
        while let Some(&due) = self.pending_reasserts.first() {
            if elapsed < due {
                break;
            }
            self.pending_reasserts = &self.pending_reasserts[1..];
            let _ = self.apply_overlay_properties();
            let _ = self.on_gl_context_ready();
        }
    }

    fn set_ignores_mouse(&mut self, ignore: bool) -> Result<(), String> {
        if self.click_through == ignore {
            return Ok(());
        }
        let w = self.win()?;
        // SAFETY: live NSWindow, documented method, BOOL argument.
        unsafe {
            let _: () = msg_send![w, setIgnoresMouseEvents: ignore];
        }
        self.click_through = ignore;
        Ok(())
    }

    /// Fully click-through: every pixel belongs to whatever is underneath.
    pub fn set_input_region_empty(&mut self) -> Result<(), String> {
        self.rects.clear();
        let w = self.win()?;
        // SAFETY: as above.
        unsafe {
            let _: () = msg_send![w, setIgnoresMouseEvents: true];
        }
        self.click_through = true;
        Ok(())
    }

    /// The blob's shape this frame.
    ///
    /// Cocoa cannot be told about a shape, so this compares the global cursor
    /// against the rectangles itself and toggles the window between clickable and
    /// invisible-to-the-mouse. Returns whether the window's state actually changed,
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
        self.set_ignores_mouse(!on_blob)?;
        Ok(was != self.click_through)
    }

    /// Give the mouse back to everyone else on the way out.
    ///
    /// Deliberately the *opposite* of the X11 version, which drops its input region
    /// and so becomes fully interactive. Here, fully interactive means a
    /// desktop-sized invisible window swallowing every click, so the safe state to
    /// leave behind for the moment between this and the window being destroyed is
    /// click-through.
    pub fn restore_input_region(&mut self) -> Result<(), String> {
        let w = self.win()?;
        // SAFETY: as above.
        unsafe {
            let _: () = msg_send![w, setIgnoresMouseEvents: true];
        }
        self.click_through = true;
        Ok(())
    }

    /// There is no region held on a server to read back. Report what the window is
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

    /// Nothing to hint. macOS has no ICCCM and no window manager that will argue
    /// about where a borderless window goes.
    pub fn set_size_hints(&self) -> Result<(), String> {
        Ok(())
    }

    /// Re-assert the window properties after it has been shown.
    ///
    /// The X11 twin of this re-sends `_NET_WM_STATE` because a window manager gets
    /// its say at map time. Cocoa is better behaved, but SDL sets a good deal of
    /// window state of its own inside `-orderFront:`, and re-applying afterwards
    /// costs one round of messages and removes a whole class of "it was transparent
    /// until it appeared" bug that would be very hard to diagnose remotely.
    pub fn nudge_state(&mut self) -> Result<(), String> {
        self.shown_at = Some(std::time::Instant::now());
        self.apply_overlay_properties()?;
        // And the surface parameter too, in case showing the window rebuilt the
        // surface underneath it. Cheap, and the failure it guards against — an
        // overlay that was transparent right up until it became visible — is one
        // that would be miserable to diagnose from the other side of an ocean.
        self.on_gl_context_ready()
    }

    /// The window covers the desktop exactly, so in its own coordinates it is always
    /// at the origin. See the note on `Mac::origin` for why this is not a lie.
    pub fn window_rect(&self) -> Result<(i32, i32, u32, u32), String> {
        Ok((0, 0, self.size.0, self.size.1))
    }

    /// Nothing to re-assert: no window manager moved us. Kept because the frame loop
    /// calls it on both platforms.
    pub fn force_full_screen_geometry(&self) -> Result<(), String> {
        Ok(())
    }

    /// Cocoa composites every window with an alpha channel, so there is no visual to
    /// get wrong and nothing to read back. Reported as 32 to mean "has alpha", which
    /// is what the strategy loop in `main.rs` is looking for.
    pub fn depth_of(&self, _handle: u64) -> Result<u8, String> {
        Ok(32)
    }

    /// The platform-specific half of the startup diagnostics.
    pub fn describe(&self, handle: u64) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        out.push(("NSWindow", format!("{handle:#018x}")));
        out.push((
            "desktop",
            format!(
                "{} x {} at ({}, {})",
                self.size.0, self.size.1, self.origin.0, self.origin.1
            ),
        ));
        // The one that fails silently, so it is stated outright. Note that a value
        // of 0 here is necessary and nowhere near sufficient — see `on_frame`.
        out.push((
            "surface opacity",
            if self.surface_made_transparent {
                format!(
                    "0 (NSOpenGLCPSurfaceOpacity), layer-backed, window alpha {WINDOW_ALPHA}, \
                     re-asserted at {REASSERT_MS:?} ms"
                )
            } else {
                "NOT SET  <-- the overlay will be a black rectangle".to_string()
            },
        ));
        out.push(("window level", format!("{WINDOW_LEVEL} (NSStatusWindowLevel)")));
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
            "whole window; the cursor is tested against the blob each frame".into(),
        ));
        out
    }
}

/// The pointer's position in SDL's global desktop coordinates.
///
/// `SDL_GetGlobalMouseState` rather than `[NSEvent mouseLocation]`: it is already in
/// the same top-left-origin space as `SDL_GetDisplayBounds`, so the cursor and the
/// window agree about where things are without a flip in between.
fn global_cursor() -> Option<(i32, i32)> {
    let mut x = 0i32;
    let mut y = 0i32;
    // SAFETY: both pointers are to live locals; SDL writes one int to each.
    unsafe {
        sdl2::sys::SDL_GetGlobalMouseState(&mut x, &mut y);
    }
    Some((x, y))
}
