# The macOS overlay

The Mac half of liquidMetal. Everything except the window itself is shared with the
Linux build: the same physics, the same shader, the same renderer, the same wire
protocol. What differs is `src/overlay_mac.rs`, which is the Cocoa answer to
`src/overlay_x11.rs`.

## Building it

```sh
xcode-select --install     # if you have never built anything on this machine
brew install cmake         # the bundled SDL2 is built with CMake
cargo run --release
```

The first build compiles SDL2 from the crate's vendored sources, which takes a
minute or two. There is nothing to install from Homebrew beyond CMake — that is what
the `bundled-sdl` feature is for, and it is on by default.

If you would rather link a system SDL2:

```sh
brew install sdl2
cargo run --release --no-default-features
```

## What this build has not been through

It was written and type-checked on Linux, against the real macOS target:

```sh
rustup target add aarch64-apple-darwin
cargo check-mac
```

That compiles every `cfg(target_os = "macos")` path with the real
`aarch64-apple-darwin` target, so the types, the Objective-C message signatures and
the SDL bindings are all checked. It does **not** link, and it has never run. So
the failure modes to expect are runtime ones — a window at the wrong level, a
property SDL overwrites, a permission macOS wants — not compile errors.

The three things most likely to be wrong on first run are listed at the bottom,
with what to do about each.

## First run: work up the ladder

Four things could be wrong and only one of them is the overlay. Run them in this
order and the first one that fails tells you which. Each step adds exactly one
layer.

```sh
cargo test                          # 1. physics + protocol. No window, no sockets.
cargo run --release -- --selftest   # 2. the simulation, scripted. Still headless.
cargo run --release -- --windowed   # 3. renderer + shader, in an ordinary window.
cargo run --release -- --net-echo   # 4. networking, headless — see below.
cargo run --release                 # 5. the overlay. Everything at once.
```

Steps 1 and 2 are pure shared code and should pass on a Mac exactly as they do on
Linux; if they do not, something is wrong with the build rather than with any of
this. Step 3 exercises the whole renderer — if the blob looks right in a normal
window, the shader, the GL context and the physics are all fine and anything left is
the overlay's fault.

Step 4 is worth doing before step 5, because it separates the network from the
window entirely. On the Mac:

```sh
cargo run --release -- --net-echo
```

and on the Linux box:

```sh
cargo run --release -- --net-serve
```

`--net-serve` invents a blob and throws it; `--net-echo` catches it and throws it
back, and the two log every round trip. Neither opens a window. If that works, then
discovery, the firewall, the local-network permission and the wire format are all
good, and step 5 is only about Cocoa.

## The three things that make a window an overlay

| | X11 | macOS |
| --- | --- | --- |
| transparent | pick a depth-32 ARGB visual by hand, then check what the driver did with it | `setOpaque:NO` + a clear background **+ `NSOpenGLCPSurfaceOpacity` = 0** |
| always on top | `_NET_WM_STATE_ABOVE` | `setLevel:` to `NSStatusWindowLevel` (25) |
| click-through except on the blob | XShape `ShapeInput` region | there is no such thing — see below |

### Transparency has the same trap, in a different place

On X11 the silent failure is `SDL_GL_ALPHA_SIZE` reporting 8 bits on a window that
has no alpha channel, because GLX will happily give you alpha bits on a depth-24
visual.

macOS has an exactly analogous trap one layer down. You can set `setOpaque:NO`, give
the window a clear background colour, get 8 real alpha bits, and still end up with a
black rectangle the size of your desktop — because the **GL surface** inside the
transparent window is still opaque. The fix is one parameter on the
`NSOpenGLContext`:

```objc
GLint zero = 0;
[ctx setValues:&zero forParameter:NSOpenGLCPSurfaceOpacity];
```

That is `Mac::on_gl_context_ready`, and it can only run once the context exists and
is current, which is why the overlay has a hook at that point on both platforms
(where X11 does nothing at all). The startup log states outright whether it
happened:

```
  surface opacity   : 0 (NSOpenGLCPSurfaceOpacity) — the GL surface composites with alpha
```

If that line says `NOT SET`, the black rectangle is why.

### There is no input region on macOS

This is the one real architectural difference, and the only place the two platforms
do genuinely different things rather than the same thing in different words.

X11 hands the server a set of rectangles and it decides, per pixel, whether a click
belongs to us. The region *is* the hit test: the app only ever hears about clicks
that landed on the blob, and everything else reaches the apps underneath without us
being involved. Cocoa has no equivalent — `setIgnoresMouseEvents:` is all-or-nothing
for the whole window.

So the region is emulated. Every frame, the global cursor position is tested against
the same rectangle cover `physics::hit_rects` builds for X11, and the window is
switched between "swallows clicks" and "invisible to the mouse". `set_input_rects`
has the same signature and the same meaning on both platforms; only the mechanism
differs.

Two consequences fall out of that, and both are handled:

- **The decision is one frame stale.** A cursor moving faster than the frame rate
  can arrive on the blob and click before the window has stopped ignoring the mouse.
  So macOS also runs the local hit test — `overlay::REGION_IS_THE_HIT_TEST` is
  `false` here, which turns on `App::check_hit`.
- **Nothing can wake an idle overlay.** While the window ignores the mouse it
  receives no events at all, so the event queue cannot tell us the pointer has
  arrived; the only way to find out is to look. `overlay::IDLE_WAIT_MS` is therefore
  30 ms here against 66 ms on X11. Each of those wake-ups is one
  `SDL_GetGlobalMouseState` and a handful of integer comparisons.

While the window is *not* ignoring the mouse it swallows clicks across its whole
surface — which is the entire desktop — so it is only made clickable when the
pointer is genuinely on the blob, and put back immediately after.

### Getting the NSWindow out of SDL

`sdl2-sys` ships **one** pre-generated `sdl_bindings.rs` for every platform, and it
was generated on Linux. The `SDL_SysWMinfo` union it declares therefore has `x11`,
`wl` and `dummy` — and no `cocoa` field, on macOS as much as anywhere else.

That is survivable, because `SDL_syswm.h` pads the union with `Uint8 dummy[64]`
precisely so its size is stable, and the macOS arm is `struct { NSWindow *window; }`
— one pointer at offset zero. Reading the first eight bytes of `dummy` is exactly
reading `info.info.cocoa.window`, and reading the `dummy` arm of a union is always
valid because `u8` has no invalid bit patterns.

The `raw-window-handle` route does **not** work here, which is worth knowing before
anyone tries it: `sdl2` builds its `AppKitWindowHandle` out of `SDL_MetalView`, and a
window created for OpenGL does not have one.

## Throwing a blob between the Mac and the Linux box

Nothing platform-specific. On both machines:

```sh
cargo run --release -- --net
```

Two things macOS will want that Linux does not:

- **A firewall prompt**, the first time, asking whether `liquid-metal` may accept
  incoming connections. It needs to be allowed, or throws will arrive nowhere and
  the blob will bounce back off the edge it left by.
- **Local network permission** (macOS 15 and later). Discovery is UDP multicast, and
  Sequoia gates that behind a per-app permission prompt. If the Mac never finds the
  Linux box, this is the first thing to check — System Settings › Privacy &
  Security › Local Network.

If discovery is blocked and you would rather not fight it, name the peer directly.
The port is in the other machine's startup log:

```sh
cargo run --release -- --peer 192.168.1.72:41521 --no-discovery
```

## If something is wrong on the first run

**A black rectangle covering the screen.** The GL surface is still opaque. Check the
`surface opacity` line in the startup log. If it says `NOT SET`, look at the error
printed just above it from `on_gl_context_ready`.

**Clicks do not reach the desktop, or the blob cannot be grabbed.** The
click-through toggle is not tracking the cursor. Run `--windowed` to confirm the
blob and the physics are fine, then check that `SDL_GetGlobalMouseState` and
`SDL_GetDisplayBounds` agree about coordinates — the whole scheme rests on those two
being in the same space, which is why both are asked of SDL rather than one of SDL
and one of `NSScreen`.

**The blob sits under the menu bar.** `WINDOW_LEVEL` in `overlay_mac.rs` is
`NSStatusWindowLevel` (25) to float above it. Dropping it to `NSFloatingWindowLevel`
(3) is the politer choice if you would rather the menu bar won.

## Settled deliberately

**The app has a Dock icon, and SDL's activation policy is left alone.** An overlay
would more usually call
`[NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory]`, which drops the
Dock icon and keeps the app out of the application switcher. That is not done here,
and the reason is that a Dock icon was judged fine — not that the question was
overlooked. If it ever stops being fine, that one message is the whole change.

Note that it would not fix the related annoyance: clicking the blob activates
liquidMetal and so deactivates whatever you were working in. Accessory policy does
not prevent activation, and preventing it properly means stopping the window
becoming key, which SDL's window class opts into.

## Known gaps

- **Displays plugged in or unplugged while running are not picked up.** The desktop
  is measured once, at startup. Restart to pick up a new monitor arrangement.
- **Retina is asked for but unverified.** The window is created with
  `allow_highdpi`, and the frame loop already scales the metaballs by the
  drawable-to-logical ratio — a path that has never had a reason to do anything on
  Linux, where that ratio is always 1.
