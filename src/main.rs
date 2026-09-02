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

#[cfg(target_os = "linux")]
#[path = "overlay_x11.rs"]
mod overlay;
#[cfg(target_os = "macos")]
#[path = "overlay_mac.rs"]
mod overlay;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "liquidMetal has an overlay for Linux (X11/XWayland) and macOS (Cocoa). \
     Porting it to another platform means writing the equivalent of src/overlay_x11.rs \
     for it: a transparent, always-on-top, click-through-except-on-the-blob window. \
     Everything else — physics, renderer, shader and the network protocol — is already \
     portable."
);

mod physics;
mod render;
mod selftest;
mod wire;
mod net;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::Keycode;
use sdl2::mouse::MouseButton;
use sdl2::video::{GLProfile, SwapInterval};

use net::{Net, NetConfig, NetEvent};
use physics::{Ball, Blob, Bounds, PointerTrack, v2};
use wire::Edge;

// ---------------------------------------------------------------------------
// TUNABLES for the shell around the simulation.
// ---------------------------------------------------------------------------

/// Frame budget while the blob is asleep: ~15 fps. It lives on someone's desktop all
/// day and must not burn a core doing nothing. An incoming event cuts the wait short,
/// so grabbing it still feels immediate.
const IDLE_WAIT_MS: u32 = overlay::IDLE_WAIT_MS;

/// Window size used by `--windowed`, the renderer debug affordance.
const WINDOWED_SIZE: (u32, u32) = (1280, 800);

/// Idle frame budget while networking is on: ~40 fps of doing nothing.
///
/// The idle path blocks on the SDL event queue, which a blob arriving over a socket
/// cannot wake. Rather than teach the network threads to push SDL events, the wait
/// is simply shortened while `--net` is on, so an incoming throw appears within a
/// frame or two. Each of those wake-ups drains an empty channel and goes back to
/// sleep; it costs nothing measurable, and only happens when networking is enabled.
const NET_IDLE_WAIT_MS: u32 = 25;

/// How many blobs one screen will hold before it starts refusing throws.
///
/// Capped by the shader's ball budget, because blobs that touch are drawn as one
/// merged field and the whole clump has to fit in one draw.
const MAX_RESIDENT_BLOBS: usize = render::MAX_BLOBS / (physics::SAT_COUNT + 1);

/// Belt and braces: a throw whose thread never reports back at all is bounced after
/// this long. The socket paths all have their own, shorter deadlines, so this only
/// fires if one of them is wedged.
const FLIGHT_TIMEOUT: Duration = Duration::from_secs(3);

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
    /// A headless peer: join the network, catch whatever is thrown here, and throw
    /// it straight back. No window, no GL, no X.
    ///
    /// This is the stand-in for a peer that is *not* this program — the thing at the
    /// other end of a real throw is expected to be a different implementation on a
    /// different operating system, and testing only this binary against itself would
    /// never exercise the protocol as a contract. It also means the whole feature can
    /// be tried out with one computer.
    NetEcho,
}

/// Everything the network layer needs, collected from the command line.
#[derive(Debug, Clone)]
struct NetOpts {
    /// Nothing binds a socket unless this is on.
    enabled: bool,
    /// Announce ourselves and listen for others.
    discovery: bool,
    group: String,
    name: Option<String>,
    capacity: usize,
    /// Raw `--peer` specs, resolved once the network starts.
    peers: Vec<String>,
    /// `--net-echo` only: how long to hold a caught blob before throwing it back.
    hold: Duration,
    /// `--net-echo` only: invent a blob and throw it at the first peer that turns
    /// up, so a game of catch can be started without anyone touching a mouse.
    serve: bool,
}

impl Default for NetOpts {
    fn default() -> NetOpts {
        NetOpts {
            enabled: false,
            discovery: true,
            group: "default".into(),
            name: None,
            capacity: MAX_RESIDENT_BLOBS,
            peers: Vec::new(),
            hold: Duration::from_millis(800),
            serve: false,
        }
    }
}

fn main() -> ExitCode {
    let mut mode = Mode::Overlay;
    let mut net = NetOpts::default();
    let mut args = std::env::args().skip(1);

    /// `--flag VALUE`, with a clear error instead of a confusing one when the
    /// value is missing.
    macro_rules! value {
        ($args:expr, $flag:expr) => {
            match $args.next() {
                Some(v) => v,
                None => {
                    eprintln!("liquidMetal: {} needs a value", $flag);
                    return ExitCode::FAILURE;
                }
            }
        };
    }

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

            // --- networking ---
            "--net" => net.enabled = true,
            "--net-echo" => {
                mode = Mode::NetEcho;
                net.enabled = true;
            }
            // Naming a peer is asking for the network; not also having to pass
            // --net removes a papercut with no ambiguity attached.
            "--peer" => {
                net.enabled = true;
                net.peers.push(value!(args, "--peer"));
            }
            "--net-group" => net.group = value!(args, "--net-group"),
            "--net-name" => net.name = Some(value!(args, "--net-name")),
            "--no-discovery" => net.discovery = false,
            "--net-capacity" => {
                let v = value!(args, "--net-capacity");
                match v.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= MAX_RESIDENT_BLOBS => net.capacity = n,
                    Ok(n) => {
                        eprintln!(
                            "liquidMetal: --net-capacity {n} is out of range; \
                             this build holds 1 to {MAX_RESIDENT_BLOBS} blobs \
                             (raise render::MAX_BLOBS for more)"
                        );
                        return ExitCode::FAILURE;
                    }
                    Err(_) => {
                        eprintln!("liquidMetal: --net-capacity wants a number, got {v:?}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--net-serve" => {
                mode = Mode::NetEcho;
                net.enabled = true;
                net.serve = true;
            }
            "--net-hold" => {
                let v = value!(args, "--net-hold");
                match v.parse::<u64>() {
                    Ok(ms) => net.hold = Duration::from_millis(ms),
                    Err(_) => {
                        eprintln!("liquidMetal: --net-hold wants milliseconds, got {v:?}");
                        return ExitCode::FAILURE;
                    }
                }
            }
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

    if mode == Mode::NetEcho {
        return match run_echo(&net) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\nliquidMetal --net-echo could not start.\n{e}");
                ExitCode::FAILURE
            }
        };
    }

    match run(&mode, &net) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\nliquidMetal could not start.\n{e}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    // A raw string rather than an escaped one: the previous form ended every line
    // with `\n    \`, and a trailing backslash in a Rust string literal eats the
    // next line's indentation, so every wrapped description silently un-indented
    // itself back to column four. What you see here is what gets printed.
    print!(
        r#"liquidMetal — a draggable liquid-metal blob for your desktop

USAGE: liquid-metal [OPTIONS]

OPTIONS:
  --windowed          Open an ordinary opaque window instead of the desktop
                      overlay. For debugging the renderer without fighting the
                      compositor.
  --selftest          Step the physics headlessly through a scripted grab /
                      fling / bounce sequence, print PASS/FAIL per assertion,
                      and exit.
  --capture P         Render a few frames, read the blob back out of the
                      framebuffer and write it to P as a NetPBM PAM file
                      (RGBA), then exit.
  -h, --help          Show this.

NETWORK — throwing the blob to another machine:
  --net               Find other machines on the network and turn the screen
                      edges that lead to one into doors. Off by default: it
                      opens a socket, and nothing about it is authenticated.
  --peer [E=]HOST:P   Name a peer explicitly, optionally pinned to the edge E
                      (left / right / top / bottom). Implies --net. Repeatable.
  --net-group NAME    Only talk to peers announcing this group. A name, not a
                      password. Default: default
  --net-name NAME     How this machine introduces itself. Default: hostname
  --no-discovery      Do not broadcast or listen for beacons; --peer only.
  --net-capacity N    Blobs this screen will hold before refusing throws (1-4).
  --net-echo          Run headless as a peer that catches a blob and throws it
                      straight back. The stand-in for a machine you do not
                      have, and the way to try this out with one computer.
  --net-serve         Like --net-echo, but invents a blob and throws it at the
                      first peer it finds. Start one of these next to a --net
                      instance and the blob arrives on its own.
  --net-hold MS       How long --net-echo holds a blob. Default: 800

CONTROLS:
  Left-drag           move the blob; release to fling it
  Double-click        reset to the centre of the screen
  Middle-click        quit (on the blob; elsewhere the click passes through)
  Esc                 quit, when the window has keyboard focus
  Ctrl+C              quit, from the terminal
"#
    );
}

fn run(mode: &Mode, netopts: &NetOpts) -> Result<(), String> {
    let overlay_mode = *mode == Mode::Overlay;
    let capture_path = match mode {
        Mode::Capture(p) => Some(p.clone()),
        _ => None,
    };

    // The X connection comes first: in overlay mode it decides how big the window
    // has to be, so it must succeed before SDL creates anything.
    let mut x11 = if overlay_mode {
        Some(overlay::Overlay::connect()?)
    } else {
        // --windowed deliberately touches no X11 at all. The whole point of that
        // mode is to debug the renderer with the compositor out of the picture.
        None
    };

    // Whatever has to be true of the process before SDL initialises. On X11 that is
    // a pair of environment variables with a long story behind them; on macOS there
    // is nothing to do.
    overlay::prepare_process_environment();

    // Deliver clicks even when the overlay does not hold keyboard focus, and do not
    // grab focus when the window is first shown.
    sdl2::hint::set("SDL_MOUSE_FOCUS_CLICKTHROUGH", "1");
    sdl2::hint::set("SDL_WINDOW_NO_ACTIVATION_WHEN_SHOWN", "1");

    let sdl = sdl2::init().map_err(|e| format!("SDL_Init failed: {e}"))?;
    let video = sdl
        .video()
        .map_err(|e| format!("the SDL video subsystem could not start: {e}"))?;

    let driver = video.current_video_driver();
    if overlay_mode {
        overlay::check_video_driver(driver)?;
    }

    // macOS cannot answer this until SDL's video subsystem is up, so the desktop is
    // probed here rather than at connect time.
    if let Some(x) = x11.as_mut() {
        x.probe_desktop(&video)?;
    }
    let (win_w, win_h) = match &x11 {
        Some(x) => x.desktop_size(),
        None => WINDOWED_SIZE,
    };
    // Where the overlay window goes. Always (0, 0) on X11, where the root window is
    // the origin; on macOS a display left of or above the primary one puts the
    // desktop's corner at negative coordinates.
    let win_origin = match &x11 {
        Some(x) => x.desktop_origin(),
        None => (0, 0),
    };

    // Getting a *transparent* window is the one genuinely driver-dependent step
    // here, so it is an ordered list of strategies, each verified against the depth
    // X actually gave the window rather than assumed to work.
    //
    // Requesting SDL_GL_ALPHA_SIZE is necessary but nowhere near sufficient: see
    // `overlay::find_argb_visual` for why the alpha bits read back as 8 on a window
    // that has no alpha channel at all.
    let argb_visual = x11.as_ref().and_then(|x| x.argb_visual);
    let attempts: Vec<GlAttempt> = if overlay_mode && overlay::NEEDS_VISUAL_STRATEGY {
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

        let (window, ctx) = match
            build_window_and_context(&video, win_origin, win_w, win_h, overlay_mode, a.ctx)
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
        let depth = match (x11.as_ref(), overlay::native_window_handle(&window)) {
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

    // Anything the overlay can only do once there is a live, current GL context.
    // Nothing on X11; on macOS this is where the GL surface is made non-opaque,
    // which is the step that decides whether the overlay is transparent or a black
    // rectangle the size of the desktop.
    if let Some(x) = x11.as_mut() {
        if let Err(e) = x.on_gl_context_ready() {
            eprintln!("[gl] the overlay surface could not be made transparent: {e}");
        }
    }

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
        xid = overlay::native_window_handle(&window)?;
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
        Some(x) => x.desktop_size(),
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
    net::publish_screen_size((bounds.x1 - bounds.x0) as u32, (bounds.y1 - bounds.y0) as u32);
    let mut blob = Blob::new(bounds);
    blob.reset();
    let mut app = App {
        cursor: blob.core.p,
        blobs: vec![Slot { blob, flight: None }],
        track: PointerTrack::new(),
        running: true,
        quit_reason: "unknown",
        logical_w,
        logical_h,
        origin,
        screen,
        geometry_dirty: false,
        touched: false,
        // On X11 the input region already decided the pointer is on the blob before
        // the event was delivered, so a second test could only disagree with it.
        // macOS has no input region — the window is toggled between clickable and
        // not, one frame behind the cursor — so the local test is what catches a
        // click that arrived in the gap.
        check_hit: !overlay_mode || !overlay::REGION_IS_THE_HIT_TEST,
        capacity: netopts.capacity,
        portals: 0,
    };

    // --- the network, if it was asked for ---
    //
    // A failure here is reported and then ignored: the blob toy works perfectly
    // well without it, and refusing to start the whole program because a socket
    // would not bind would be a poor trade.
    let mut net = if netopts.enabled && capture_path.is_none() {
        match start_net(netopts) {
            Ok(n) => {
                println!(
                    "  network           : on as {:?} in group {:?}, listening on {} \
                     (holding up to {} blob(s))",
                    netopts.name.clone().unwrap_or_else(net::hostname),
                    netopts.group,
                    n.local_addr,
                    netopts.capacity,
                );
                println!(
                    "                      screen edges that lead to a peer become doors; \
                     throw the blob off one."
                );
                Some(n)
            }
            Err(e) => {
                eprintln!("[net] networking is off: {e}");
                None
            }
        }
    } else {
        None
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
        if app.at_rest() {
            idle_frames += 1;
            let wait = if net.is_some() { NET_IDLE_WAIT_MS } else { IDLE_WAIT_MS };
            if let Some(ev) = event_pump.wait_event_timeout(wait) {
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
        if app.grabbed().is_some() {
            app.track.push(start.elapsed().as_secs_f64(), app.cursor);
        }

        // --- fixed-timestep accumulator, decoupled from the render rate ---
        let now = Instant::now();
        let frame_dt = (now - last).as_secs_f32().min(physics::MAX_FRAME_TIME);
        last = now;
        acc += frame_dt;
        // Which edges lead somewhere. Re-asked every frame, because a peer
        // appearing or going away turns a wall into a door and back.
        let portals = net.as_ref().map(|n| n.portal_edges()).unwrap_or(0);
        app.set_portals(portals);

        while acc >= physics::SUBSTEP {
            for slot in app.blobs.iter_mut() {
                // A blob that has left and is waiting for its receipt is frozen
                // once it is fully out of sight, so a throw that is slow to be
                // answered does not coast away to infinity and come back from
                // somewhere absurd if it is refused.
                if slot.flight.is_some() && slot.blob.is_off_screen() {
                    continue;
                }
                slot.blob.step(physics::SUBSTEP);
            }
            acc -= physics::SUBSTEP;
        }

        // --- the network: things leaving, things arriving ---
        if let Some(n) = net.as_mut() {
            n.set_resident(app.resident());
            handle_departures(&mut app, n);
            let events = n.poll();
            handle_net_events(&mut app, n, events);
            reap_stalled_flights(&mut app);
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
                            for slot in app.blobs.iter_mut() {
                                slot.blob.reset();
                            }
                            if let Some(first) = app.blobs.first() {
                                app.cursor = first.blob.core.p;
                            }
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
            // The union across every blob. XShape takes the rectangles as a set,
            // so concatenating each blob's own cover is already the right answer.
            let mut rects: Vec<(i32, i32, i32, i32)> = Vec::new();
            for slot in &app.blobs {
                if slot.flight.is_some() && slot.blob.is_off_screen() {
                    continue;
                }
                rects.extend(slot.blob.hit_rects());
            }
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

        // Blobs whose bounding boxes touch are shaded in one pass, so they join into
        // a single metaball field and merge like the liquid they are supposed to be.
        // Blobs far apart get their own pass, scissored to their own corner, and
        // cost exactly what one blob has always cost.
        let mut groups: Vec<(Bounds, Vec<Ball>)> = Vec::new();
        for slot in &app.blobs {
            if slot.flight.is_some() && slot.blob.is_off_screen() {
                continue;
            }
            let mut balls: Vec<Ball> = slot.blob.balls().to_vec();
            let bb = slot.blob.bbox();
            if (dpr - 1.0).abs() > 1e-3 {
                for b in balls.iter_mut() {
                    b.p = b.p * dpr;
                    b.r *= dpr;
                }
            }
            groups.push((bb, balls));
        }
        merge_touching(&mut groups);

        // --capture uses an ordinary window for convenience, but must render the
        // transparent path, otherwise the alpha it reads back is meaningless.
        let opaque = !overlay_mode && capture_path.is_none();
        let time = start.elapsed().as_secs_f32();
        renderer.begin_frame(draw_w as i32, draw_h as i32);
        if opaque {
            // The opaque debug path draws its checkerboard everywhere, so it is one
            // unscissored pass over every ball on screen.
            let all: Vec<Ball> = groups.iter().flat_map(|(_, b)| b.iter().copied()).collect();
            renderer.draw_group(draw_w as i32, draw_h as i32, time, &all, None, true);
        } else {
            for (bb, balls) in &groups {
                renderer.draw_group(
                    draw_w as i32,
                    draw_h as i32,
                    time,
                    balls,
                    Some(scissor_of(*bb, dpr)),
                    false,
                );
            }
        }
        // --capture reads back one rectangle, so it wants the union of everything.
        let scissor = scissor_of(union_all(&groups), dpr);
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

/// One blob on this screen, plus whatever the network is currently doing with it.
struct Slot {
    blob: Blob,
    /// Set once the blob has gone through a door and we are waiting for the peer's
    /// receipt. Until that arrives the blob is still ours: still simulated, still
    /// drawn sliding off the edge, and still recoverable.
    flight: Option<Flight>,
}

/// A throw in progress.
struct Flight {
    throw_id: u64,
    /// The edge it left by, so a refusal can be turned back into a bounce off that
    /// same edge.
    edge: Edge,
    peer: String,
    sent: Instant,
}

/// Everything the event handler mutates, so it can be one method instead of a
/// nine-argument function.
struct App {
    /// Blobs on this screen. Ordinarily one; more once machines start throwing
    /// them at each other. Newest last, which is also grab priority.
    blobs: Vec<Slot>,
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
    /// How many blobs this screen will hold.
    capacity: usize,
    /// Edges that currently lead to a peer, so an arriving blob is given the same
    /// doors as everyone else.
    portals: u8,
}

impl App {
    /// Which blob is under `at`, if any.
    ///
    /// `hit_test` uses the very same rectangle cover the X input region is built
    /// from, so in overlay mode this agrees with the decision X already made about
    /// whether the click reaches us at all — it is only being asked *which* blob.
    fn blob_at(&self, at: physics::Vec2) -> Option<usize> {
        // Last first: the most recently arrived blob is on top of an older one it
        // has landed on.
        if let Some(i) =
            self.blobs.iter().rposition(|s| s.flight.is_none() && s.blob.hit_test(at))
        {
            return Some(i);
        }
        // In overlay mode X has already decided the pointer is on a blob, so a miss
        // here means the input region is a frame stale and the blob has moved since
        // it was uploaded. Give the click to whichever blob the pointer is deepest
        // inside rather than dropping it — a dropped grab on a fast blob is much
        // more annoying than a slightly generous one.
        if !self.check_hit {
            return self
                .blobs
                .iter()
                .enumerate()
                .filter(|(_, s)| s.flight.is_none())
                .max_by(|a, b| a.1.blob.field(at).total_cmp(&b.1.blob.field(at)))
                .map(|(i, _)| i);
        }
        None
    }

    fn grabbed(&self) -> Option<usize> {
        self.blobs.iter().position(|s| s.blob.is_grabbed())
    }

    /// Blobs actually on this screen, which is what the peer is told and what the
    /// capacity check uses. A blob in flight has already left.
    fn resident(&self) -> usize {
        self.blobs.iter().filter(|s| s.flight.is_none()).count()
    }

    /// True when nothing is moving and nothing is in the post, so the frame rate
    /// can be throttled.
    fn at_rest(&self) -> bool {
        self.blobs.iter().all(|s| s.flight.is_none() && s.blob.is_at_rest())
    }

    /// Re-derive every blob's world after the window has moved or been resized.
    fn rebound(&mut self) {
        let b = visible_bounds(self.origin, (self.logical_w, self.logical_h), self.screen);
        for s in self.blobs.iter_mut() {
            s.blob.set_bounds(b);
        }
        net::publish_screen_size((b.x1 - b.x0) as u32, (b.y1 - b.y0) as u32);
    }

    /// The blob's world, recomputed from where the window actually is.
    fn bounds(&self) -> Bounds {
        visible_bounds(self.origin, (self.logical_w, self.logical_h), self.screen)
    }

    /// Open or close the doors. Called every frame: a peer appearing or vanishing
    /// changes an edge from a wall to a door and back.
    fn set_portals(&mut self, mask: u8) {
        self.portals = mask;
        for s in self.blobs.iter_mut() {
            // A blob already on its way out keeps its door open even if the peer
            // has just gone; closing it under a departing blob would strand it
            // outside the screen with no wall to come back to.
            if s.flight.is_none() {
                s.blob.set_portals(mask);
            }
        }
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
                if self.blob_at(self.cursor).is_some() {
                    self.stop("middle-click on the blob");
                }
            }

            Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, clicks, .. } => {
                self.cursor = v2(x as f32, y as f32);
                let Some(i) = self.blob_at(self.cursor) else { return };
                self.touched = true;
                if clicks >= 2 {
                    self.blobs[i].blob.reset();
                    mouse.capture(false);
                    self.track.clear();
                } else {
                    self.track.clear();
                    self.track.push(t, self.cursor);
                    self.blobs[i].blob.grab(self.cursor);
                    // Without capture, a fast drag outruns the input region: the
                    // pointer leaves it, motion events stop, and the blob is
                    // dropped mid-fling.
                    mouse.capture(true);
                }
            }

            Event::MouseButtonUp { mouse_btn: MouseButton::Left, x, y, .. } => {
                if let Some(i) = self.grabbed() {
                    self.cursor = v2(x as f32, y as f32);
                    self.track.push(t, self.cursor);
                    let throw = self.track.velocity(t);
                    self.blobs[i].blob.release(throw);
                }
                mouse.capture(false);
                self.track.clear();
            }

            Event::MouseMotion { x, y, .. } => {
                self.cursor = v2(x as f32, y as f32);
                if let Some(i) = self.grabbed() {
                    self.track.push(t, self.cursor);
                    self.blobs[i].blob.drag_to(self.cursor);
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
// ---------------------------------------------------------------------------
// Drawing several blobs
// ---------------------------------------------------------------------------

/// The scissor rectangle for a bounding box, in framebuffer pixels, with a couple
/// of pixels of slack for the antialiased rim.
fn scissor_of(bb: Bounds, dpr: f32) -> (i32, i32, i32, i32) {
    (
        (bb.x0 * dpr).floor() as i32 - 2,
        (bb.y0 * dpr).floor() as i32 - 2,
        ((bb.x1 - bb.x0) * dpr).ceil() as i32 + 4,
        ((bb.y1 - bb.y0) * dpr).ceil() as i32 + 4,
    )
}

fn union_all(groups: &[(Bounds, Vec<Ball>)]) -> Bounds {
    let mut it = groups.iter().map(|(b, _)| *b);
    match it.next() {
        Some(first) => it.fold(first, union_of),
        None => Bounds { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 },
    }
}

fn union_of(a: Bounds, b: Bounds) -> Bounds {
    Bounds {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

fn touches(a: Bounds, b: Bounds) -> bool {
    a.x0 <= b.x1 && b.x0 <= a.x1 && a.y0 <= b.y1 && b.y0 <= a.y1
}

/// Fold groups whose bounding boxes touch into one, repeatedly, until none do.
///
/// Balls shaded in the same pass share one metaball field, so this is what decides
/// whether two blobs that meet flow together or merely overlap. Quadratic and
/// restarted on every merge, which is fine for the handful of blobs a screen holds
/// and much easier to be sure of than a union-find.
fn merge_touching(groups: &mut Vec<(Bounds, Vec<Ball>)>) {
    let mut again = true;
    while again {
        again = false;
        'outer: for i in 0..groups.len() {
            for j in (i + 1)..groups.len() {
                if touches(groups[i].0, groups[j].0)
                    && groups[i].1.len() + groups[j].1.len() <= render::MAX_BLOBS
                {
                    let (bb, balls) = groups.remove(j);
                    groups[i].0 = union_of(groups[i].0, bb);
                    groups[i].1.extend(balls);
                    again = true;
                    break 'outer;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Throwing and catching
// ---------------------------------------------------------------------------

fn start_net(opts: &NetOpts) -> Result<Net, String> {
    let mut pinned = Vec::new();
    for spec in &opts.peers {
        let (edge, addr) =
            net::parse_peer(spec).map_err(|e| format!("--peer {spec:?} is not usable: {e}"))?;
        pinned.push((edge, addr));
    }
    Net::start(NetConfig {
        group: opts.group.clone(),
        name: opts.name.clone().unwrap_or_else(net::hostname),
        capacity: opts.capacity as u16,
        discovery: opts.discovery,
        pinned,
    })
}

/// Any blob that has gone through a door this frame becomes a throw.
///
/// The velocity handed to the wire is divided by *this* screen's height, and the
/// receiver multiplies by its own — see `wire.rs` for why the gesture rather than
/// the pixels is what travels.
fn handle_departures(app: &mut App, net: &mut Net) {
    for slot in app.blobs.iter_mut() {
        if slot.flight.is_some() {
            continue;
        }
        let Some(edge) = slot.blob.departing_edge() else { continue };
        let b = slot.blob.bounds;
        let h = (b.y1 - b.y0).max(1.0);
        let along = slot.blob.along_edge(edge);
        let v = slot.blob.core.v;
        match net.throw(edge, along, (v.x / h, v.y / h), slot.blob.sat_states()) {
            Some((throw_id, peer)) => {
                let label = peer.label();
                println!(
                    "[net] the blob went out the {} edge at {:.0}%, {:.0} px/s -> {label}",
                    edge.name(),
                    along * 100.0,
                    v.len()
                );
                slot.flight =
                    Some(Flight { throw_id, edge, peer: label, sent: Instant::now() });
            }
            None => {
                // The peer went away between the door opening and the blob getting
                // to it. That edge is a wall again.
                slot.blob.bounce_back(edge);
            }
        }
    }
}

/// Turn a throw that did not connect back into a bounce.
fn bounce_flight(app: &mut App, throw_id: u64, why: &str) {
    if let Some(slot) = app
        .blobs
        .iter_mut()
        .find(|s| s.flight.as_ref().is_some_and(|f| f.throw_id == throw_id))
    {
        let flight = slot.flight.take().expect("just matched on it");
        println!("[net] the throw to {} did not connect ({why}); it bounced", flight.peer);
        slot.blob.bounce_back(flight.edge);
    }
}

fn handle_net_events(app: &mut App, net: &mut Net, events: Vec<NetEvent>) {
    for ev in events {
        match ev {
            NetEvent::Arrived { throw, from } => {
                // The network layer reserved a slot before acknowledging, so there
                // is room; this check is the belt to that braces.
                if app.blobs.len() >= app.capacity.max(1) + 1 {
                    net.commit_arrival();
                    eprintln!("[net] dropped a blob from {from}: no room after all");
                    continue;
                }
                let bounds = app.bounds();
                let h = (bounds.y1 - bounds.y0).max(1.0);
                let entry = throw.edge.opposite();
                let vel = v2(throw.vel_x * h, throw.vel_y * h);
                let mut blob = Blob::arriving(bounds, entry, throw.along, vel, &throw.sats);
                blob.set_portals(app.portals);
                println!(
                    "[net] caught a blob from {from}: in at the {} edge, {:.0}% along, \
                     {:.0} px/s, {} satellites of shape",
                    entry.name(),
                    throw.along * 100.0,
                    vel.len(),
                    throw.sats.len()
                );
                app.blobs.push(Slot { blob, flight: None });
                app.touched = true;
                net.commit_arrival();
            }
            NetEvent::Landed { throw_id, peer } => {
                let before = app.blobs.len();
                app.blobs
                    .retain(|s| s.flight.as_ref().map(|f| f.throw_id) != Some(throw_id));
                if app.blobs.len() < before {
                    println!("[net] {peer} has it.");
                }
            }
            NetEvent::Refused { throw_id, peer: _, why } => {
                bounce_flight(app, throw_id, why.explain());
            }
            NetEvent::Lost { throw_id, peer: _, why } => bounce_flight(app, throw_id, &why),
            NetEvent::PeerUp { name, addr, screen } => {
                println!(
                    "[net] {name} is here at {addr} ({}x{}). The edges that lead to it \
                     are doors now.",
                    screen.0, screen.1
                );
            }
            NetEvent::PeerDown { name } => {
                println!("[net] {name} has gone.");
            }
            NetEvent::Note(m) => eprintln!("[net] {m}"),
        }
    }
}

/// A throw whose thread never reported back at all. Every socket path has its own
/// shorter deadline, so this should never fire — but a blob stuck off-screen with
/// no way home would be the worst possible bug to leave available.
fn reap_stalled_flights(app: &mut App) {
    let stalled: Vec<u64> = app
        .blobs
        .iter()
        .filter_map(|s| s.flight.as_ref())
        .filter(|f| f.sent.elapsed() > FLIGHT_TIMEOUT)
        .map(|f| f.throw_id)
        .collect();
    for id in stalled {
        bounce_flight(app, id, "no answer at all");
    }
}

// ---------------------------------------------------------------------------
// --net-echo
// ---------------------------------------------------------------------------

/// A peer with no screen: catch a blob, hold it, throw it back.
///
/// The point of this mode is that the thing at the other end of a real throw will
/// not be this program. Running the graphical app against *itself* would happily
/// pass a broken assumption back and forth and never notice. This end has no
/// physics, no renderer and no window — only the protocol — so anything it can
/// catch and return is genuinely defined by `wire.rs` and not by shared code.
fn run_echo(opts: &NetOpts) -> Result<(), String> {
    /// A plausible screen, so `along` and the velocity units are exercised for
    /// real rather than degenerating to 1.
    const ECHO_SCREEN: (u32, u32) = (1920, 1080);

    net::publish_screen_size(ECHO_SCREEN.0, ECHO_SCREEN.1);
    let mut net = start_net(opts)?;
    println!(
        "liquidMetal --net-echo: a headless peer in group {:?}, listening on {}.\n\
         Pretending to be a {}x{} screen. Catching blobs and throwing them straight \
         back after {} ms.\n\
         Ctrl+C to stop.\n",
        opts.group,
        net.local_addr,
        ECHO_SCREEN.0,
        ECHO_SCREEN.1,
        opts.hold.as_millis()
    );

    let mut held: Vec<(Instant, wire::Throw)> = Vec::new();
    let mut caught = 0u64;
    let mut returned = 0u64;
    let mut served = !opts.serve;

    loop {
        net.set_resident(held.len());

        // Put a blob into play. Nothing here has a screen or a simulation, so the
        // served blob is invented outright — a throw off the right-hand edge, half
        // way up, at a bit over one screen-height per second, carrying a plausibly
        // deformed set of satellites.
        if !served {
            let sats: Vec<wire::SatState> = (0..8)
                .map(|i| {
                    let a = std::f32::consts::TAU * (i as f32) / 8.0;
                    wire::SatState {
                        off_x: a.cos() * 1.3,
                        off_y: a.sin() * 0.8,
                        vel_x: -0.4,
                        vel_y: 0.1,
                    }
                })
                .collect();
            if net.throw(Edge::Right, 0.5, (1.2, -0.15), sats).is_some() {
                served = true;
                println!("[echo] served a blob off the right edge");
            }
        }
        for ev in net.poll() {
            match ev {
                NetEvent::Arrived { throw, from } => {
                    caught += 1;
                    println!(
                        "[echo] caught #{caught} from {from}: out of their {} edge at \
                         {:.0}%, {:.2} screen-heights/s, {} satellites",
                        throw.edge.name(),
                        throw.along * 100.0,
                        (throw.vel_x * throw.vel_x + throw.vel_y * throw.vel_y).sqrt(),
                        throw.sats.len()
                    );
                    held.push((Instant::now(), throw));
                    net.commit_arrival();
                }
                NetEvent::Landed { peer, .. } => {
                    returned += 1;
                    println!("[echo] thrown back; {peer} has it ({returned} returned)");
                }
                NetEvent::Refused { peer, why, .. } => {
                    eprintln!("[echo] {peer} refused it: {}", why.explain());
                }
                NetEvent::Lost { peer, why, .. } => {
                    eprintln!("[echo] the throw back to {peer} was lost: {why}");
                }
                NetEvent::PeerUp { name, screen, .. } => {
                    println!("[echo] {name} is here ({}x{})", screen.0, screen.1);
                }
                NetEvent::PeerDown { name } => println!("[echo] {name} has gone"),
                NetEvent::Note(m) => eprintln!("[echo] {m}"),
            }
        }

        // Anything held long enough goes straight back the way it came: out of the
        // edge it arrived through, with its velocity reversed. On the far screen
        // that reads as the blob coming back in the edge it left by.
        let now = Instant::now();
        let mut still_held = Vec::with_capacity(held.len());
        for (at, throw) in held.drain(..) {
            if now.duration_since(at) < opts.hold {
                still_held.push((at, throw));
                continue;
            }
            let back = throw.edge.opposite();
            if net
                .throw(back, throw.along, (-throw.vel_x, -throw.vel_y), throw.sats.clone())
                .is_none()
            {
                // Nobody to throw it to yet. Hold on to it and try again; the peer
                // may simply not have been heard from since it restarted.
                eprintln!("[echo] nowhere to throw it back to; still holding");
                still_held.push((now, throw));
            }
        }
        held = still_held;

        std::thread::sleep(Duration::from_millis(10));
    }
}

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
    origin: (i32, i32),
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
        builder.position(origin.0, origin.1).borderless().hidden();
        // Retina: ask for the full backing resolution. The frame loop already
        // scales the metaballs by the drawable-to-logical ratio, so this is the
        // difference between a crisp blob and a blurry one on a Mac laptop.
        builder.allow_highdpi();
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

#[allow(clippy::too_many_arguments)]
fn print_diagnostics(
    video: &sdl2::VideoSubsystem,
    window: &sdl2::video::Window,
    renderer: &render::Renderer,
    x11: &Option<overlay::Overlay>,
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
        // Everything platform-specific about the window is the overlay's to
        // describe. The X11 and macOS versions have almost nothing in common to say,
        // and this is the seam where that stops mattering.
        Some(x) => {
            for (label, value) in x.describe(xid) {
                println!("  {label:<18}: {value}");
            }
        }
        None => println!("  desktop overlay   : not used (--windowed)"),
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
