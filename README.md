# liquidMetal

A blob of polished liquid metal that floats on top of your desktop. Grab it with the
mouse, fling it, and it coasts with inertia, stretching along its velocity and
wobbling when it settles. Everywhere except the blob itself, clicks pass straight
through to whatever is behind it.

![the blob](doc/blob.png)

## Running it

```sh
cargo run --release
```

The first build compiles SDL2 from source (~30 s) — there are no SDL development
headers on this machine, so the `sdl2` crate's `bundled` + `static-link` features
build it from the vendored sources.

| Flag | What it does |
| --- | --- |
| *(none)* | The desktop overlay. This is the product. |
| `--windowed` | An ordinary opaque window with a checkerboard behind the blob. For working on the renderer without fighting the compositor. |
| `--capture <path>` | Render a few frames, read the blob back out of the framebuffer, write it to `<path>` as a NetPBM PAM (RGBA, premultiplied), exit. The lever that makes the renderer checkable without looking at a screen. |
| `--selftest` | Step the physics headlessly through a scripted grab / fling / bounce, print PASS/FAIL per assertion, exit non-zero on failure. |

```sh
cargo test                              # 12 physics unit tests
cargo run --release -- --selftest       # 8 scripted-simulation assertions
```

## Controls

| Input | Action |
| --- | --- |
| Left-drag | Move the blob; release to fling it |
| Double-click | Reset to the centre of the screen |
| Middle-click on the blob | Quit |
| `Esc` | Quit, when the window has keyboard focus |
| `Ctrl+C` | Quit, from the terminal (SDL turns `SIGINT` into `SDL_QUIT`) |

Because the click-through region *is* the hit test, the app only ever sees a click
that lands on the blob. Everything else goes to the apps underneath, so middle-click
and `Esc` only reach us when we are genuinely the target.

## The KDE / XWayland assumption

This runs as an **X11 client**, on XWayland when your session is Wayland. SDL has no
Wayland layer-shell support, so an always-on-top click-through overlay is not
reachable from a Wayland-native SDL surface. The program forces `SDL_VIDEODRIVER=x11`
on itself at startup and refuses to run on any other driver rather than silently
producing a window that behaves wrongly.

It also **unsets `WAYLAND_DISPLAY` for its own process only**. Forcing SDL's video
driver is not enough: EGL picks its *platform* by sniffing the environment, and with
`WAYLAND_DISPLAY` still set it selects the Wayland platform and segfaults inside
`libnvidia-egl-wayland` because we never made a Wayland connection.

KWin composites XWayland ARGB windows correctly and honours `_NET_WM_STATE_ABOVE` for
them. The window sets `_NET_WM_STATE_ABOVE`, `_STICKY`, `_SKIP_TASKBAR`,
`_SKIP_PAGER` and `_NET_WM_WINDOW_TYPE_UTILITY` — deliberately *not* `_DOCK`, which
would make KWin reserve screen-edge struts and shove your other windows aside.

### Window placement: the other thing KWin argues about

The overlay wants to be at (0, 0) at the full size of the X virtual screen, so the
blob can be dragged between monitors. KWin runs its own placement policy on a
borderless utility window and drops it at the **primary monitor's origin** — on this
dual-head desktop that is x=1920, which pushes the right half of a 3840-wide window
clean off the display. The blob would then coast into the part of the window that is
not on any screen and vanish.

`WM_NORMAL_HINTS` with the ICCCM `USPosition` flag (the "the user asked for exactly
this" flag, which window managers honour where they override a mere program request)
is set before mapping, and KWin ignores it at map time. Asking again with a
`ConfigureWindow` once the window has settled *does* stick, so the program polls its
real geometry from X for the first 90 frames and re-asserts up to ten times. The
startup log traces it:

```
[x11] the window manager placed us at (1920, 0) 3840x1080 rather than (0, 0) ...
[x11] window is now 3840x1080 at (0, 0)
```

Two things make this safe rather than a race:

- **The X server is the only source of truth for geometry.** SDL's `Moved` event
  reports (0, 0) for a window the X server says is at (1920, 0) under XWayland, so
  that event is used only to mark the geometry stale; the numbers come from
  `TranslateCoordinates`.
- **The blob's world is the intersection of the window with the virtual screen**, in
  window-relative coordinates. If a window manager ever refuses to move us, the blob
  is confined to the part of the window that is actually visible rather than allowed
  to wander off-screen. It just cannot then be dragged onto a monitor the window does
  not cover, and the log says so.

### Transparency: the part that is genuinely fiddly

`SDL_GL_SetAttribute(SDL_GL_ALPHA_SIZE, 8)` is **necessary but not sufficient**, and
the way it fails is silent. GLX reports alpha bits for the *GL colour buffer*, which
depth-24 X visuals happily claim — this machine has 640 such fbconfigs against 32
real depth-32 ones. SDL takes `glXChooseFBConfig(...)[0]`, lands on a depth-24 visual,
and you get a window with **no alpha channel**: `SDL_GL_ALPHA_SIZE` reads back as `8`
while your whole screen turns opaque black behind the blob.

So the program picks a depth-32 TrueColor visual itself and hands it to SDL via
`SDL_VIDEO_X11_WINDOW_VISUALID`, then **asks X what depth the window actually got**
and says so in the startup log. It tries three strategies in order and keeps the
first that yields depth 32:

1. **ARGB visual + 3.3 core context.** Fails with `BadMatch` on this NVIDIA driver,
   because SDL builds a core context from its own independently-chosen fbconfig,
   which need not match our window's visual.
2. **ARGB visual + compatibility context.** ← what works here. Asking for a pre-3.0
   version with no profile mask is the one SDL path that builds the context from the
   *window's* visual, so the two always agree. NVIDIA answers with a 4.6
   compatibility context, which runs the `#version 330 core` shader unchanged.
3. **SDL's own visual.** Runs, but opaque. Prints a loud warning.

If you see `X window depth : 24` in the log, that is the failure, and it is stated
explicitly rather than left for you to discover by looking at a black screen.

## Layout

```
src/main.rs      event loop, fixed-timestep accumulator, GL/visual strategy, wiring
src/overlay.rs   X11: ARGB visual discovery, EWMH properties, XShape input region
src/physics.rs   blob soft-body sim; pure, no SDL/GL/X types, unit-tested
src/render.rs    GL context, shader compile, uniform upload, draw, framebuffer readback
src/shader.frag  the metal shader
src/selftest.rs  the scripted --selftest run
```

`physics.rs` has no SDL, GL or X dependency at all. It is the only part that can be
exercised headlessly, so it is kept that way on purpose.

## How it works

**Physics.** A core particle plus 8 satellites, each spring-attached to a rest offset
on a circle around the core, all rendered as one metaball union. The satellites
lagging behind the core *is* the stretch; their spring ring-down *is* the wobble.
Neither is faked separately. Dragging drives the core with a stiff damped spring
rather than teleporting it — the lag is the good part. Release velocity is averaged
over the last 80 ms of pointer track, which is most of what makes flinging feel good.
Simulation runs at a fixed 240 Hz regardless of frame rate.

**Click-through.** The XShape `ShapeInput` region is recomputed each frame by
rasterising the field onto a 14 px grid, thresholding below the visible isosurface,
dilating one cell, and merging horizontal runs — about 15 rectangles. `ShapeBounding`
is deliberately *not* used: it would clip the rendered pixels and destroy the
antialiased rim. The region is only re-uploaded when it actually changes, and the
grid quantisation is what makes that rare. While dragging, `SDL_CaptureMouse` keeps
the pointer events coming even when a fast drag outruns the region.

**Shading.** Metaball field → analytic gradient → surface normal → procedural studio
environment reflection plus Fresnel. No diffuse term: it is a metal, so everything
you see is reflection. The edge is antialiased from `fwidth` of the field, so it
stays a ~2 px ramp at any resolution, and the shader outputs **premultiplied** alpha
because X compositors expect it.

Two things in there are worth knowing if you edit it:

- The normal is built from the field evaluated with a **softened core**
  (`NORMAL_SOFT`). The raw `1/d²` field has a singularity at every ball centre, and a
  normal built from it carves a visible bead into the surface at each satellite — the
  blob reads as eight balls in a bag rather than one pool of metal. Far from the
  balls the softened and raw fields agree, so the silhouette is untouched.
- The environment's horizon is tilted well off the view axis (`r.z * 0.42`). With no
  tilt, the blob's centre sits exactly on the horizon band and the whole environment
  funnels into a single point in the middle.

## Tuning the feel

Every constant is in one marked block at the top of its module.

`src/physics.rs`:

| Constant | Effect |
| --- | --- |
| `SPRING_K`, `SPRING_DAMP` | Satellite springs. Lower damping = more wobble, longer ring-down. |
| `GRAB_K`, `GRAB_DAMP` | How tightly the blob follows the cursor. Lower `GRAB_K` = more lag, more elastic drag. |
| `DRAG_K`, `FRICTION` | Coast. `DRAG_K` is the exponential decay that gives the long glide; `FRICTION` is the small constant deceleration that actually brings it to a stop in finite time. |
| `RESTITUTION`, `IMPACT_KICK`, `IMPACT_RIPPLE_PX` | Wall bounce and how visibly the impact ripples through the body. |
| `MAX_STRETCH`, `STRETCH_PER_SPEED` | Squash and stretch. Area is preserved. |
| `CORE_RADIUS`, `SAT_RADIUS`, `SAT_ORBIT` | Size and shape. **If you change these, run `cargo test`** — `collide_radius_matches_the_rendered_surface` will tell you `COLLIDE_RADIUS` needs to move with them. The metaball union is much larger than any single ball and cannot be eyeballed. |
| `HIT_CELL`, `HIT_ISO`, `HIT_DILATE` | Size of the grabbable margin around the blob. |
| `IDLE_WAIT_MS` (in `main.rs`) | Idle frame rate. Defaults to ~15 fps when the blob is asleep; an incoming event cuts the wait short so it still feels immediate. |

`src/shader.frag`: `FLOOR_COLOR`, `SKY_COLOR`, `HORIZON_COLOR/WIDTH/GAIN`,
`STRIP_*` (the azimuthal softbox strips — elevation-only environments look like a
cheap gradient, azimuthal structure is most of what sells it as a real reflection),
`KEY_*` / `FILL_*` (the sharp specular highlights), `METAL_TINT`, `EXPOSURE`,
`RIPPLE_AMOUNT` (keep it restrained: this is chrome, not lava), `NORMAL_POW` (>1
flattens the interior toward a pool rather than a ball bearing).

Iterating on the look is fast:

```sh
cargo run --release -- --capture /tmp/blob.pam && magick /tmp/blob.pam /tmp/blob.png
```

## Known limitations

- **No refraction or distortion of the desktop behind the blob.** A transparent
  overlay cannot sample what is underneath it without screen capture, so this is not
  attempted.
- **The overlay does not get a 3.3 *core* context on this driver**, only a 4.6
  compatibility one — see the transparency section above. Behaviour is identical for
  everything this program does; `--windowed` does get a real 3.3 core context, so the
  shader is exercised against core-profile GLSL there.
- **If a window manager refuses to move the overlay to (0, 0)**, the blob is confined
  to the visible part of the window and cannot be dragged onto monitors the window
  does not cover. KWin relents on retry here, so this is a fallback rather than the
  normal path; the startup log says which happened.
- **Keyboard focus.** The window asks not to take focus on map (`_NET_WM_USER_TIME`
  of 0 plus SDL's `SDL_WINDOW_NO_ACTIVATION_WHEN_SHOWN`), which means `Esc` only
  works once you have clicked the blob. Middle-click and `Ctrl+C` always work.
