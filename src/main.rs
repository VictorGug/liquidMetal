//! liquidMetal — a draggable blob of liquid metal that floats on your desktop.
//!
//! Runs as an X11 client (XWayland is the intended host on KDE/Wayland): an ARGB,
//! borderless, always-on-top window covering the whole X virtual screen, whose
//! XShape *input* region is trimmed each frame to just the blob. Everywhere else,
//! clicks fall straight through to whatever is underneath.
//!
//! Decision taken once, at the top: the crate stays on edition 2024, and the single
//! `std::env::set_var` call in `main` is wrapped in `unsafe`. Downgrading the whole
//! crate to edition 2021 to avoid one `unsafe` block was not worth it.

mod overlay;
mod physics;
mod render;
mod selftest;

use std::process::ExitCode;
use std::time::Instant;

use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::Keycode;
use sdl2::mouse::MouseButton;
use sdl2::video::{GLProfile, SwapInterval};

use physics::{Blob, Bounds, PointerTrack, v2};

// ---------------------------------------------------------------------------
// TUNABLES for the shell around the simulation.
// ---------------------------------------------------------------------------

/// Frame budget while the blob is asleep: ~15 fps. It lives on someone's desktop all
/// day and must not burn a core doing nothing. An incoming event cuts the wait short,
/// so grabbing it still feels immediate.
const IDLE_WAIT_MS: u32 = 66;

/// Window size used by `--windowed`, the renderer debug affordance.
const WINDOWED_SIZE: (u32, u32) = (1280, 800);

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Overlay,
    Windowed,
    SelfTest,
    /// Render a few frames, read the blob's pixels back out of the framebuffer, and
    /// write them to a file as RGBA. The lever that makes the renderer checkable
    /// without anyone being able to look at the screen.
    Capture(String),
}

fn main() -> ExitCode {
    let mut mode = Mode::Overlay;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--windowed" => mode = Mode::Windowed,
            "--selftest" => mode = Mode::SelfTest,
            "--capture" => match args.next() {
                Some(path) => mode = Mode::Capture(path),
                None => {
                    eprintln!("liquidMetal: --capture needs a file path");
                    return ExitCode::FAILURE;
                }
            },
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("liquidMetal: unknown argument {other:?}");
                print_usage();
                return ExitCode::FAILURE;
            }
        }
    }

    if mode == Mode::SelfTest {
        return if selftest::run() { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }

    match run(&mode) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\nliquidMetal could not start.\n{e}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!(
        "liquidMetal — a draggable liquid-metal blob for your desktop\n\
         \n\
         USAGE: liquid-metal [OPTIONS]\n\
         \n\
         OPTIONS:\n    \
           --windowed   Open an ordinary opaque window instead of the desktop overlay.\n    \
                        For debugging the renderer without fighting the compositor.\n    \
           --selftest   Step the physics headlessly through a scripted grab / fling /\n    \
                        bounce sequence, print PASS/FAIL per assertion, and exit.\n    \
           --capture P  Render a few frames, read the blob back out of the framebuffer\n    \
                        and write it to P as a NetPBM PAM file (RGBA), then exit.\n    \
           -h, --help   Show this.\n\
         \n\
         CONTROLS:\n    \
           Left-drag       move the blob; release to fling it\n    \
           Double-click    reset to the centre of the screen\n    \
           Middle-click    quit (on the blob; everywhere else the click passes through)\n    \
           Esc             quit, when the window has keyboard focus\n    \
           Ctrl+C          quit, from the terminal"
    );
}

fn run(mode: &Mode) -> Result<(), String> {
    let overlay_mode = *mode == Mode::Overlay;
    let capture_path = match mode {
        Mode::Capture(p) => Some(p.clone()),
        _ => None,
    };

    // The X connection comes first: in overlay mode it decides how big the window
    // has to be, so it must succeed before SDL creates anything.
    let mut x11 = if overlay_mode {
        Some(overlay::X11::connect()?)
    } else {
        // --windowed deliberately touches no X11 at all. The whole point of that
        // mode is to debug the renderer with the compositor out of the picture.
        None
    };

    // SDL has no Wayland layer-shell support, so an always-on-top click-through
    // overlay is not reachable from a Wayland-native surface. Force the X11 backend
    // (XWayland) before SDL_Init looks at the environment.
    //
    // SAFETY: single-threaded, and this runs before any SDL or std call that could
    // concurrently read the environment.
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

    // Deliver clicks even when the overlay does not hold keyboard focus, and do not
    // grab focus when the window is first shown.
    sdl2::hint::set("SDL_MOUSE_FOCUS_CLICKTHROUGH", "1");
    sdl2::hint::set("SDL_WINDOW_NO_ACTIVATION_WHEN_SHOWN", "1");

    let sdl = sdl2::init().map_err(|e| format!("SDL_Init failed: {e}"))?;
    let video = sdl
        .video()
        .map_err(|e| format!("the SDL video subsystem could not start: {e}"))?;

    let driver = video.current_video_driver();
    if driver != "x11" {
        return Err(format!(
            "SDL chose the {driver:?} video driver, but liquidMetal needs {:?}.\n\
             The overlay relies on X11 features (ARGB visuals, EWMH, XShape) that have \
             no SDL-reachable equivalent on Wayland.\n\
             Check that an X server or XWayland is running on $DISPLAY.",
            "x11"
        ));
    }

    let (win_w, win_h) = match &x11 {
        Some(x) => (x.virtual_w as u32, x.virtual_h as u32),
        None => WINDOWED_SIZE,
    };

    // Getting a *transparent* window is the one genuinely driver-dependent step
    // here, so it is an ordered list of strategies, each verified against the depth
    // X actually gave the window rather than assumed to work.
    //
    // Requesting SDL_GL_ALPHA_SIZE is necessary but nowhere near sufficient: see
    // `overlay::find_argb_visual` for why the alpha bits read back as 8 on a window
    // that has no alpha channel at all.
    let argb_visual = x11.as_ref().and_then(|x| x.argb_visual);
    let attempts: Vec<GlAttempt> = if overlay_mode {
        vec![
            GlAttempt {
                visual: argb_visual,
                ctx: CtxKind::Core33,
                label: "ARGB visual + 3.3 core context",
            },
            GlAttempt {
                visual: argb_visual,
                ctx: CtxKind::LegacyCompat,
                label: "ARGB visual + compatibility context",
            },
            // Last resort: whatever SDL picks. Runs, but opaque.
            GlAttempt {
                visual: None,
                ctx: CtxKind::Core33,
                label: "SDL's own visual + 3.3 core context",
            },
        ]
    } else {
        vec![GlAttempt {
            visual: None,
            ctx: CtxKind::Core33,
            label: "SDL's own visual + 3.3 core context",
        }]
    };

    let mut built: Option<(sdl2::video::Window, sdl2::video::GLContext)> = None;
    let mut chosen = "";
    let mut last_err = String::new();
    for (i, a) in attempts.iter().enumerate() {
        let is_last = i + 1 == attempts.len();
        sdl2::hint::set(
            "SDL_VIDEO_X11_WINDOW_VISUALID",
            &a.visual.map(|v| v.to_string()).unwrap_or_default(),
        );

        let (window, ctx) = match build_window_and_context(&video, win_w, win_h, overlay_mode, a.ctx)
        {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[gl] {} did not work: {e}", a.label);
                last_err = e;
                continue;
            }
        };

        // The honest check: ask X what depth the window really got. Depth 32 is the
        // only thing that gives the overlay an alpha channel.
        let depth = match (x11.as_ref(), x_window_id(&window)) {
            (Some(x), Ok(id)) => x.depth_of(id).unwrap_or(0),
            _ => 0,
        };
        if depth == 32 || is_last {
            if overlay_mode && depth != 32 {
                eprintln!(
                    "\n*** WARNING: no strategy produced a 32-bit X window. ***\n\
                     The window is depth {depth}, so it has no alpha channel and the\n\
                     overlay will render as an OPAQUE rectangle covering your screen.\n\
                     Use --windowed to work on the renderer instead.\n"
                );
            }
            built = Some((window, ctx));
            chosen = a.label;
            break;
        }
        eprintln!("[gl] {} gave a depth-{depth} window (no alpha); trying the next", a.label);
        drop(ctx);
        drop(window);
    }

    let (mut window, gl_ctx) = built.ok_or_else(|| {
        format!(
            "{last_err}\n\
             liquidMetal needs an OpenGL 3.3 core context. Check your driver with \
             `glxinfo -B`."
        )
    })?;
    window
        .gl_make_current(&gl_ctx)
        .map_err(|e| format!("the OpenGL context could not be made current: {e}"))?;

    let got_alpha = video.gl_attr().alpha_size();
    let got_rgb = (
        video.gl_attr().red_size(),
        video.gl_attr().green_size(),
        video.gl_attr().blue_size(),
    );
    let got_ver = video.gl_attr().context_version();
    let got_profile = video.gl_attr().context_profile();

    if overlay_mode && got_alpha == 0 {
        eprintln!(
            "\n*** WARNING: SDL_GL_ALPHA_SIZE came back as 0 bits. ***\n\
             The framebuffer has no alpha channel, so the overlay will render as an \n\
             OPAQUE rectangle covering your whole screen instead of a floating blob.\n\
             This means the X server did not offer a 32-bit ARGB visual. Try \
             --windowed to confirm the renderer itself is fine.\n"
        );
    }

    let mut renderer = render::Renderer::new(|s| video.gl_get_proc_address(s) as *const _)?;

    let swap_ok = video.gl_set_swap_interval(SwapInterval::VSync).is_ok();
    if !swap_ok {
        eprintln!("[sdl] warning: vsync could not be enabled; the frame rate is uncapped");
    }

    // --- overlay setup: properties, then an empty input region, then map ---
    let mut xid = 0u64;
    if let Some(x) = x11.as_mut() {
        xid = x_window_id(&window)?;
        x.set_window(xid)?;
        x.apply_overlay_properties()?;
        // Fully click-through from the very first frame it is visible.
        x.set_input_region_empty()?;
        x.set_size_hints()?;
        window.show();
        x.nudge_state()?;
        // A window manager is free to move or resize us on map, and KWin does
        // constrain new windows to a single monitor's work area. Ask for the whole
        // virtual screen back; whatever we actually end up with is read below and
        // becomes the blob's world, so the walls always match what is drawn.
        x.force_full_screen_geometry()?;
    }

    print_diagnostics(
        &video, &window, &renderer, &x11, xid, got_alpha, got_rgb, got_ver, got_profile, swap_ok,
        mode, chosen,
    );

    // --- state ---
    // Trust the X server over our own request: `window.size()` reports what SDL
    // asked for, `window_rect()` reports what the window manager actually gave us.
    // KWin moves a borderless utility window to the primary monitor's origin, which
    // on a dual-head desktop is x=1920 — pushing the right half of a
    // full-virtual-screen window clean off the display. WM_NORMAL_HINTS asks it not
    // to; this is what happens when it does anyway.
    let (mut logical_w, mut logical_h) = window.size();
    let mut origin = (0i32, 0i32);
    let screen = match x11.as_ref() {
        Some(x) => (x.virtual_w as u32, x.virtual_h as u32),
        None => (logical_w, logical_h),
    };
    if let Some(x) = x11.as_ref() {
        match x.window_rect() {
            Ok((wx, wy, ww, wh)) if ww > 0 && wh > 0 => {
                if (wx, wy) != (0, 0) || (ww, wh) != (logical_w, logical_h) {
                    eprintln!(
                        "[x11] the window manager placed us at ({wx}, {wy}) {ww}x{wh} rather \
                         than (0, 0) {logical_w}x{logical_h}; will ask again shortly"
                    );
                }
                origin = (wx, wy);
                logical_w = ww;
                logical_h = wh;
            }
            Ok(_) => {}
            Err(e) => eprintln!("[x11] could not read the window geometry: {e}"),
        }
    }
    let bounds = visible_bounds(origin, (logical_w, logical_h), screen);
    let mut blob = Blob::new(bounds);
    blob.reset();
    let mut app = App {
        cursor: blob.core.p,
        blob,
        track: PointerTrack::new(),
        running: true,
        quit_reason: "unknown",
        logical_w,
        logical_h,
        origin,
        screen,
        geometry_dirty: false,
        touched: false,
        check_hit: !overlay_mode,
    };
    let mouse = sdl.mouse();
    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| format!("the SDL event pump could not be created: {e}"))?;

    let start = Instant::now();
    let mut last = start;
    let mut acc = 0.0f32;
    let mut frames = 0u64;
    let mut idle_frames = 0u64;
    let mut region_verified = false;
    // KWin places a borderless utility window on the primary monitor at map time and
    // ignores WM_NORMAL_HINTS' USPosition when doing it. Asking again once it has
    // settled often sticks where the request at map time did not, so try a handful
    // of times and then accept whatever we have.
    let mut geometry_fixups = 0u32;
    let mut geometry_polls = 0u32;
    const MAX_GEOMETRY_FIXUPS: u32 = 10;
    /// Frames to poll X for our real geometry before trusting SDL's events alone.
    const GEOMETRY_POLL_FRAMES: u32 = 90;

    while app.running {
        // While asleep, block on the event queue instead of spinning. An event cuts
        // the wait short, so the blob is still immediately grabbable.
        if app.blob.is_at_rest() {
            idle_frames += 1;
            if let Some(ev) = event_pump.wait_event_timeout(IDLE_WAIT_MS) {
                app.handle(ev, &mouse, &start);
            }
        }
        for ev in event_pump.poll_iter() {
            app.handle(ev, &mouse, &start);
        }
        if !app.running {
            break;
        }

        // Keep feeding the throw estimator even when the pointer is not moving, so
        // holding still for a moment before letting go really does drop the blob.
        if app.blob.is_grabbed() {
            app.track.push(start.elapsed().as_secs_f64(), app.cursor);
        }

        // --- fixed-timestep accumulator, decoupled from the render rate ---
        let now = Instant::now();
        let frame_dt = (now - last).as_secs_f32().min(physics::MAX_FRAME_TIME);
        last = now;
        acc += frame_dt;
        while acc >= physics::SUBSTEP {
            app.blob.step(physics::SUBSTEP);
            acc -= physics::SUBSTEP;
        }

        // --- keep the blob's world in step with where the window really is ---
        if let Some(x) = x11.as_ref() {
            // Poll X directly for a short while after startup, and thereafter only
            // when SDL says something changed. Two round-trips, so not every frame
            // forever.
            if geometry_polls < GEOMETRY_POLL_FRAMES || app.geometry_dirty {
                geometry_polls += 1;
                app.geometry_dirty = false;
                if let Ok((wx, wy, ww, wh)) = x.window_rect() {
                    if ww > 0
                        && wh > 0
                        && ((wx, wy) != app.origin || (ww, wh) != (app.logical_w, app.logical_h))
                    {
                        eprintln!("[x11] window is now {ww}x{wh} at ({wx}, {wy})");
                        app.origin = (wx, wy);
                        app.logical_w = ww;
                        app.logical_h = wh;
                        app.rebound();
                        // The blob was centred in whatever we thought the screen was
                        // a moment ago. If nobody has grabbed it yet, put it back in
                        // the middle of the real one. (`is_at_rest` is no good here:
                        // it needs a quarter second of stillness that has not elapsed
                        // this early in startup.)
                        if !app.touched {
                            app.blob.reset();
                            app.cursor = app.blob.core.p;
                        }
                        // Re-log the input region: the one reported before the move
                        // describes a window that is no longer where it was.
                        region_verified = false;
                    }
                }
                // KWin places a borderless utility window on the primary monitor at
                // map time and ignores WM_NORMAL_HINTS' USPosition doing it. Asking
                // again once it has settled sometimes sticks where the request at
                // map time did not.
                if app.origin != (0, 0) && geometry_fixups < MAX_GEOMETRY_FIXUPS {
                    geometry_fixups += 1;
                    let _ = x.force_full_screen_geometry();
                    if geometry_fixups == MAX_GEOMETRY_FIXUPS {
                        eprintln!(
                            "[x11] the window manager insists on ({}, {}); confining the blob \
                             to the visible part of the window. It cannot be dragged onto a \
                             monitor the window does not cover.",
                            app.origin.0, app.origin.1
                        );
                    }
                }
            }
        }

        // --- input region: the region *is* the hit test, so it is the only one ---
        if let Some(x) = x11.as_mut() {
            let rects = app.blob.hit_rects();
            match x.set_input_rects(&rects) {
                Err(e) => {
                    eprintln!("[x11] could not update the input region: {e}");
                    app.stop("the X input region could not be updated");
                    break;
                }
                Ok(uploaded) => {
                    // Once, on the first upload, read the region back off the server
                    // rather than assuming the request landed.
                    if uploaded && !region_verified {
                        region_verified = true;
                        match x.read_back_input_region() {
                            Ok(server) => {
                                let bbox = server.iter().fold(
                                    (i32::MAX, i32::MAX, i32::MIN, i32::MIN),
                                    |a, r| {
                                        (
                                            a.0.min(r.0 as i32),
                                            a.1.min(r.1 as i32),
                                            a.2.max(r.0 as i32 + r.2 as i32),
                                            a.3.max(r.1 as i32 + r.3 as i32),
                                        )
                                    },
                                );
                                println!(
                                    "  input region      : {} rects sent, {} held by the server, \
                                     covering ({}, {}) to ({}, {})",
                                    rects.len(),
                                    server.len(),
                                    bbox.0,
                                    bbox.1,
                                    bbox.2,
                                    bbox.3
                                );
                            }
                            Err(e) => eprintln!("[x11] could not read the input region back: {e}"),
                        }
                    }
                }
            }
        }

        // --- render ---
        let (draw_w, draw_h) = window.drawable_size();
        // Physics and pointer coordinates are logical pixels; the framebuffer may be
        // larger under a HiDPI scale, so scale the metaballs on the way to the shader.
        let dpr = if app.logical_w > 0 { draw_w as f32 / app.logical_w as f32 } else { 1.0 };
        let mut balls = app.blob.balls();
        if (dpr - 1.0).abs() > 1e-3 {
            for b in balls.iter_mut() {
                b.p = b.p * dpr;
                b.r *= dpr;
            }
        }
        let bb = app.blob.bbox();
        let scissor = (
            (bb.x0 * dpr).floor() as i32 - 2,
            (bb.y0 * dpr).floor() as i32 - 2,
            ((bb.x1 - bb.x0) * dpr).ceil() as i32 + 4,
            ((bb.y1 - bb.y0) * dpr).ceil() as i32 + 4,
        );
        // --capture uses an ordinary window for convenience, but must render the
        // transparent path, otherwise the alpha it reads back is meaningless.
        let opaque = !overlay_mode && capture_path.is_none();
        renderer.draw(
            draw_w as i32,
            draw_h as i32,
            start.elapsed().as_secs_f32(),
            &balls,
            Some(scissor),
            opaque,
        );
        // --capture: let the shader settle for a few frames, then read the blob's
        // own bounding box straight out of the framebuffer, alpha included.
        if let Some(path) = &capture_path {
            if frames >= 3 {
                let x = scissor.0.max(0);
                let y = scissor.1.max(0);
                let cw = scissor.2.min(draw_w as i32 - x);
                let ch = scissor.3.min(draw_h as i32 - y);
                let px = renderer.read_pixels(x, y, cw, ch, draw_h as i32);
                write_pam(path, cw as u32, ch as u32, &px)?;
                println!(
                    "captured {cw} x {ch} px at ({x}, {y}) to {path} \
                     (NetPBM PAM, RGB_ALPHA, premultiplied)"
                );
                app.stop("--capture finished");
                break;
            }
        }

        window.gl_swap_window();
        frames += 1;
    }

    // --- clean shutdown ---
    if let Some(x) = x11.as_mut() {
        if let Err(e) = x.restore_input_region() {
            eprintln!("[x11] could not restore the input region: {e}");
        }
    }
    drop(renderer);
    drop(gl_ctx);
    // Explicitly, rather than leaving it to process teardown, so the compositor sees
    // the window go away promptly.
    unsafe { sdl2::sys::SDL_DestroyWindow(window.raw()) };
    std::mem::forget(window);

    let secs = start.elapsed().as_secs_f32();
    println!(
        "liquidMetal exited cleanly: {}.\n\
         {frames} frames / {secs:.1} s ({:.0} fps average; {idle_frames} of those were \
         throttled idle frames).",
        app.quit_reason,
        frames as f32 / secs.max(1e-3)
    );
    Ok(())
}

/// Everything the event handler mutates, so it can be one method instead of a
/// nine-argument function.
struct App {
    blob: Blob,
    track: PointerTrack,
    cursor: physics::Vec2,
    running: bool,
    /// Why the loop stopped, so an unexpected exit is never a silent mystery.
    quit_reason: &'static str,
    logical_w: u32,
    logical_h: u32,
    /// Where the window sits on the root, in screen pixels.
    origin: (i32, i32),
    /// The X virtual screen, so the blob can be kept inside the part of the window
    /// that is actually visible.
    screen: (u32, u32),
    /// Whether the user has grabbed the blob yet. Until they have, the blob is
    /// still sitting where we put it, so it is safe to re-centre when we discover
    /// the window is not where we thought it was.
    touched: bool,
    /// SDL says the window moved or resized. SDL's idea of *where* it is disagrees
    /// with the X server's under XWayland, so this only marks the geometry stale;
    /// the real numbers are re-read from X.
    geometry_dirty: bool,
    /// In overlay mode the XShape input region already decided that the pointer is
    /// on the blob before X delivered this event, so there is nothing to re-test —
    /// and a second test here could only ever *disagree* with the region. Windowed
    /// mode has no input region, so it needs one, and it uses the exact same
    /// rectangle cover to guarantee the two modes behave identically.
    check_hit: bool,
}

impl App {
    fn on_blob(&self) -> bool {
        !self.check_hit || self.blob.hit_test(self.cursor)
    }

    /// Re-derive the blob's world after the window has moved or been resized.
    fn rebound(&mut self) {
        let b = visible_bounds(self.origin, (self.logical_w, self.logical_h), self.screen);
        self.blob.set_bounds(b);
    }

    fn stop(&mut self, why: &'static str) {
        self.running = false;
        self.quit_reason = why;
    }

    fn handle(&mut self, ev: Event, mouse: &sdl2::mouse::MouseUtil, start: &Instant) {
        let t = start.elapsed().as_secs_f64();
        match ev {
            // SDL turns SIGINT/SIGTERM into SDL_QUIT, so Ctrl+C arrives here too.
            Event::Quit { .. } => self.stop("SDL_QUIT (SIGINT/SIGTERM or window closed)"),

            Event::KeyDown { keycode: Some(Keycode::Escape), .. } => self.stop("Esc pressed"),

            Event::MouseButtonDown { mouse_btn: MouseButton::Middle, x, y, .. } => {
                self.cursor = v2(x as f32, y as f32);
                if self.on_blob() {
                    self.stop("middle-click on the blob");
                }
            }

            Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, clicks, .. } => {
                self.cursor = v2(x as f32, y as f32);
                if !self.on_blob() {
                    return;
                }
                self.touched = true;
                if clicks >= 2 {
                    self.blob.reset();
                    mouse.capture(false);
                    self.track.clear();
                } else {
                    self.track.clear();
                    self.track.push(t, self.cursor);
                    self.blob.grab(self.cursor);
                    // Without capture, a fast drag outruns the input region: the
                    // pointer leaves it, motion events stop, and the blob is
                    // dropped mid-fling.
                    mouse.capture(true);
                }
            }

            Event::MouseButtonUp { mouse_btn: MouseButton::Left, x, y, .. } => {
                if self.blob.is_grabbed() {
                    self.cursor = v2(x as f32, y as f32);
                    self.track.push(t, self.cursor);
                    let throw = self.track.velocity(t);
                    self.blob.release(throw);
                }
                mouse.capture(false);
                self.track.clear();
            }

            Event::MouseMotion { x, y, .. } => {
                self.cursor = v2(x as f32, y as f32);
                if self.blob.is_grabbed() {
                    self.track.push(t, self.cursor);
                    self.blob.drag_to(self.cursor);
                }
            }

            Event::Window { win_event: WindowEvent::SizeChanged(w, h), .. }
            | Event::Window { win_event: WindowEvent::Resized(w, h), .. } => {
                if w > 0 && h > 0 {
                    self.logical_w = w as u32;
                    self.logical_h = h as u32;
                    if self.check_hit {
                        // --windowed: the window *is* the world.
                        self.screen = (w as u32, h as u32);
                    }
                    self.geometry_dirty = true;
                    self.rebound();
                }
            }

            // The window manager can move us at any time, not just on map. Do not
            // believe the coordinates in this event: under XWayland SDL reports
            // (0, 0) for a window the X server says is at (1920, 0). Just mark the
            // geometry stale and let the main loop ask X.
            Event::Window { win_event: WindowEvent::Moved(..), .. } => {
                self.geometry_dirty = true;
            }

            _ => {}
        }
    }
}

/// One strategy for obtaining a window plus a GL context that agree with each other.
#[derive(Clone, Copy)]
struct GlAttempt {
    visual: Option<u32>,
    ctx: CtxKind,
    label: &'static str,
}

/// The blob's world: the part of the window that is actually on screen, in
/// window-relative pixels.
///
/// Rendering and pointer coordinates are both window-relative, so confining the blob
/// to this rectangle keeps it visible and grabbable no matter where the window
/// manager decided to put the window. When the window sits exactly over the virtual
/// screen — the case we ask for — this is simply the whole window.
fn visible_bounds(origin: (i32, i32), win: (u32, u32), screen: (u32, u32)) -> Bounds {
    let x0 = (-origin.0).max(0);
    let y0 = (-origin.1).max(0);
    let x1 = (screen.0 as i32 - origin.0).min(win.0 as i32);
    let y1 = (screen.1 as i32 - origin.1).min(win.1 as i32);
    // A degenerate intersection would leave the blob nowhere to live; fall back to
    // the whole window rather than trap it in a zero-width box.
    if x1 - x0 < 2 * physics::COLLIDE_RADIUS as i32 || y1 - y0 < 2 * physics::COLLIDE_RADIUS as i32
    {
        return Bounds::screen(win.0 as f32, win.1 as f32);
    }
    Bounds { x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32 }
}

/// Write RGBA8 pixels as a NetPBM PAM file.
///
/// PAM because it is the only trivially-hand-writable format that carries a real
/// alpha channel, and ImageMagick / PIL both read it. No image crate needed.
fn write_pam(path: &str, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)
        .map_err(|e| format!("could not create {path}: {e}"))?;
    write!(
        f,
        "P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
    )
    .map_err(|e| format!("could not write {path}: {e}"))?;
    f.write_all(rgba)
        .map_err(|e| format!("could not write {path}: {e}"))
}

/// Which kind of GL context to ask SDL for.
#[derive(Clone, Copy, PartialEq)]
enum CtxKind {
    /// An explicit 3.3 core context. SDL builds this with `glXCreateContextAttribsARB`
    /// from its *own* `glXChooseFBConfig(...)[0]`, which is chosen independently of
    /// our window and on this driver lands on a depth-24 fbconfig — so pairing it
    /// with a forced 32-bit visual can fail with BadMatch.
    Core33,
    /// No profile mask and a pre-3.0 version. This is the one SDL path that builds
    /// the context from the *window's* visual (`glXCreateContext(display, vinfo, ..)`),
    /// so it always agrees with a forced ARGB visual. NVIDIA answers with a 4.6
    /// compatibility context, which runs the `#version 330 core` shader unchanged.
    LegacyCompat,
}

/// Create the window and its GL context together, so a visual that GLX refuses can
/// be retried as one unit.
fn build_window_and_context(
    video: &sdl2::VideoSubsystem,
    w: u32,
    h: u32,
    overlay_mode: bool,
    ctx_kind: CtxKind,
) -> Result<(sdl2::video::Window, sdl2::video::GLContext), String> {
    // These must be set *before* window creation: SDL uses them to choose the visual.
    {
        let a = video.gl_attr();
        match ctx_kind {
            CtxKind::Core33 => {
                a.set_context_profile(GLProfile::Core);
                a.set_context_version(3, 3);
            }
            CtxKind::LegacyCompat => {
                // A zero profile mask and major < 3 are exactly the conditions under
                // which SDL takes the legacy `glXCreateContext` path.
                a.set_context_profile(GLProfile::Unknown(0));
                a.set_context_version(2, 1);
            }
        }
        a.set_red_size(8);
        a.set_green_size(8);
        a.set_blue_size(8);
        // Necessary but not sufficient for a transparent window; see the strategy
        // list in `run` and `overlay::find_argb_visual`.
        a.set_alpha_size(8);
        a.set_depth_size(0);
        a.set_stencil_size(0);
        a.set_double_buffer(true);
    }

    let mut builder = video.window("liquidMetal", w, h);
    builder.opengl();
    if overlay_mode {
        // Created hidden so the EWMH properties and an empty input region are in
        // place before the window manager ever sees it mapped.
        builder.position(0, 0).borderless().hidden();
    } else {
        builder.position_centered().resizable();
    }
    let window = builder
        .build()
        .map_err(|e| format!("the window could not be created: {e}"))?;
    let ctx = window
        .gl_create_context()
        .map_err(|e| format!("the GL context could not be created: {e}"))?;
    Ok((window, ctx))
}

/// Pull the X11 window id out of SDL.
///
/// Route (a) from the two available: `SDL_GetWindowWMInfo` with a version-stamped
/// `SDL_SysWMinfo`. Chosen over the `raw-window-handle` route because that one
/// panics internally when the query fails, and window setup is exactly where a
/// panic is least useful.
fn x_window_id(window: &sdl2::video::Window) -> Result<u64, String> {
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

#[allow(clippy::too_many_arguments)]
fn print_diagnostics(
    video: &sdl2::VideoSubsystem,
    window: &sdl2::video::Window,
    renderer: &render::Renderer,
    x11: &Option<overlay::X11>,
    xid: u64,
    alpha_bits: u8,
    rgb_bits: (u8, u8, u8),
    ctx_version: (u8, u8),
    profile: GLProfile,
    vsync: bool,
    mode: &Mode,
    gl_strategy: &str,
) {
    let (w, h) = window.size();
    let (dw, dh) = window.drawable_size();
    let v = sdl2::version::version();
    println!("=== liquidMetal ===");
    println!("  mode              : {mode:?}");
    println!("  SDL               : {}.{}.{} (linked)", v.major, v.minor, v.patch);
    println!("  SDL video driver  : {}", video.current_video_driver());
    println!("  GL strategy       : {gl_strategy}");
    println!("  GL version        : {}", renderer.gl_version);
    println!("  GL renderer       : {}", renderer.gl_renderer);
    println!("  GL vendor         : {}", renderer.gl_vendor);
    println!("  GLSL version      : {}", renderer.glsl_version);
    // These read back what SDL was *asked* for, not what the driver returned; the
    // GL version line above is the authority on what we actually got.
    println!(
        "  GL attrs asked    : version {}.{}, profile mask {}",
        ctx_version.0,
        ctx_version.1,
        match profile {
            GLProfile::Core => "core".to_string(),
            GLProfile::Compatibility => "compatibility".to_string(),
            GLProfile::GLES => "ES".to_string(),
            GLProfile::Unknown(i) => format!("unset ({i}) -> driver picks"),
        }
    );
    println!(
        "  colour bits       : R{} G{} B{} A{}{}",
        rgb_bits.0,
        rgb_bits.1,
        rgb_bits.2,
        alpha_bits,
        if alpha_bits == 0 { "   <-- NO ALPHA, see warning above" } else { "" }
    );
    println!("  vsync             : {}", if vsync { "on" } else { "unavailable" });
    println!("  window size       : {w} x {h} logical, {dw} x {dh} drawable");
    match x11 {
        Some(x) => {
            println!("  window XID        : {xid:#010x} ({xid})");
            match x.window_rect() {
                Ok((wx, wy, ww, wh)) => println!(
                    "  window on screen  : {ww} x {wh} at ({wx}, {wy}){}",
                    if (wx, wy) == (0, 0) && (ww, wh) == (x.virtual_w as u32, x.virtual_h as u32) {
                        "  [matches the virtual screen]"
                    } else {
                        "  <-- NOT the full virtual screen"
                    }
                ),
                Err(e) => println!("  window on screen  : unknown ({e})"),
            }
            // The authoritative transparency check: GL alpha bits are not enough,
            // the X window itself has to be depth 32.
            match x.window_depth() {
                Ok(32) => println!(
                    "  X window depth    : 32  <-- ARGB visual {}, alpha channel present",
                    x.argb_visual
                        .map(|v| format!("{v:#x}"))
                        .unwrap_or_else(|| "(SDL's choice)".into())
                ),
                Ok(d) => println!(
                    "  X window depth    : {d}  <-- NO ALPHA CHANNEL: the overlay will be opaque"
                ),
                Err(e) => println!("  X window depth    : unknown ({e})"),
            }
            println!(
                "  XShape version    : {}.{}",
                x.shape_version.0, x.shape_version.1
            );
            println!(
                "  X virtual screen  : {} x {} at (0, 0)",
                x.virtual_w, x.virtual_h
            );
            if x.monitors.is_empty() {
                println!("  monitors          : (RandR 1.5 GetMonitors unavailable)");
            } else {
                for m in &x.monitors {
                    println!(
                        "  monitor           : {:<12} {:>5} x {:<5} at ({:>5}, {:>5}){}",
                        m.name,
                        m.width,
                        m.height,
                        m.x,
                        m.y,
                        if m.primary { "  [primary]" } else { "" }
                    );
                }
            }
            println!("  input region      : empty (whole window click-through)");
        }
        None => println!("  X11               : not used (--windowed)"),
    }
    // Measured off the actual field rather than assumed, so the log tells you what
    // is really on screen if you have been retuning the radius constants.
    let probe = physics::Blob::new(physics::Bounds::screen(1000.0, 1000.0));
    println!(
        "  blob              : {} metaballs, {:.0} px across, grabbable to {:.0} px",
        probe.balls().len(),
        probe.iso_radius(1.0) * 2.0,
        probe.iso_radius(physics::HIT_ISO) * 2.0
    );
    println!("  controls          : left-drag move / fling, double-click reset, middle-click quit, Esc quit");
    println!("===================");
}
