# The Windows overlay

The Windows half of liquidMetal. Everything except the window itself is shared with
the Linux and macOS builds: same physics, same shader, same renderer, same wire
protocol. What differs is `src/overlay_win.rs`.

## Building it

You need the **MSVC toolchain** (`rustup default stable-msvc`, which is the default
on Windows), **CMake**, and the **Visual Studio C++ build tools**. CMake is what
compiles the vendored SDL2; the C++ build tools are what CMake compiles it with.

```powershell
winget install Kitware.CMake
winget install Microsoft.VisualStudio.2022.BuildTools   # "Desktop development with C++"
cargo run --release
```

Nothing else — SDL2 is built from the crate's vendored sources and statically
linked, so there is no DLL to place next to the executable.

## What this build has not been through

It was written and type-checked on Linux against the real target:

```powershell
rustup target add x86_64-pc-windows-msvc
cargo check-win
```

That compiles every `cfg(target_os = "windows")` path with the real
`x86_64-pc-windows-msvc` target, so the types and the Win32 signatures are checked.
It does **not** link, and it has never run. Expect runtime problems, not compile
errors — and see the ladder below, which narrows down which layer is at fault.

The macOS overlay went through exactly this and needed one substantive fix on first
run. Assume the same here.

## First run: work up the ladder

Four things could be wrong and only one of them is the overlay. Run them in this
order; the first that fails tells you which.

```powershell
cargo test                       # 1. physics + protocol. No window, no sockets.
cargo blob --selftest            # 2. the simulation, scripted. Still headless.
cargo blob --windowed            # 3. renderer + shader, in an ordinary window.
cargo net-echo                   # 4. networking, headless.
cargo run --release              # 5. the overlay. Everything at once.
```

Step 3 is the important one: if the blob looks right in a normal window, then the
shader, the GL context and the physics are all fine and anything left is the
overlay's fault. For step 4, run `cargo net-serve` on the other machine — neither
end opens a window, so it tests discovery, the firewall and the wire format on
their own.

## The three things that make a window an overlay

| | X11 | macOS | Windows |
| --- | --- | --- | --- |
| transparent | a depth-32 ARGB visual | `setOpaque:NO` + surface opacity 0 | DWM blur-behind with an **empty** blur region |
| always on top | `_NET_WM_STATE_ABOVE` | `setLevel:` | `WS_EX_TOPMOST` |
| click-through except on the blob | XShape `ShapeInput` | toggle `ignoresMouseEvents` | toggle `WS_EX_TRANSPARENT` |

### Transparency: an empty blur region is the whole trick

`DwmEnableBlurBehindWindow` sounds like the wrong function and the name is a
historical accident. What it does that matters here is switch the window onto DWM's
**per-pixel alpha** path, after which the alpha the shader writes is the alpha the
desktop composites with. The blur is a separate thing, requested by the region you
hand it, and Windows 8 dropped it anyway.

So the region is deliberately empty — `CreateRectRgn(0, 0, -1, -1)`, a rectangle
with negative extent:

```rust
let region = CreateRectRgn(0, 0, -1, -1);
let bb = DWM_BLURBEHIND {
    dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
    fEnable: 1,
    hRgnBlur: region,
    fTransitionOnMaximized: 0,
};
DwmEnableBlurBehindWindow(hwnd, &bb);
```

Blur nothing, but respect alpha everywhere. Passing a *null* region instead asks for
the whole window to be blurred, which is not what is wanted and on Windows 8 and
later does nothing at all.

The startup log says outright whether DWM accepted it:

```
  DWM alpha         : on (blur-behind, empty region) — the window composites with alpha
```

If that line says `FAILED`, it carries the HRESULT, and the overlay will be opaque.

### Why not WS_EX_LAYERED

`WS_EX_LAYERED` is the other route to a transparent window and is deliberately not
used. A layered window gets its pixels from `UpdateLayeredWindow` — which wants to be
handed a bitmap, and there is no bitmap here, the frame lives in an OpenGL back
buffer that `SwapBuffers` presents — or from `SetLayeredWindowAttributes`, which only
does uniform alpha and colour keys, not per-pixel. On some drivers combining it with
GL gives a window that never paints at all.

If the overlay turns out to be opaque and DWM reports success, this is nevertheless
the first thing to try changing: add `WS_EX_LAYERED` to `EX_STYLE_ON` and call
`SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA)` after it.

### Click-through works the way it does on macOS

Windows has `SetWindowRgn`, which is a real region and does per-pixel hit testing —
but like X11's `ShapeBounding` it clips the *rendering* too, which would hard-edge
the antialiased rim. So it is not used, and the region is emulated exactly as on
macOS: every frame the global cursor is tested against the same rectangle cover
`physics::hit_rects` builds for X11, and `WS_EX_TRANSPARENT` is turned on or off to
match.

The same two consequences follow, handled the same way. The decision is one frame
stale, so `REGION_IS_THE_HIT_TEST` is `false` and the local hit test stays on. And a
window with `WS_EX_TRANSPARENT` receives no mouse messages at all, so nothing can
wake an idle overlay — `IDLE_WAIT_MS` is 30 here against 66 on X11, and each of those
wake-ups is one `GetCursorPos` and a few integer comparisons.

### DPI awareness is not optional

`prepare_process_environment` sets `SDL_WINDOWS_DPI_AWARENESS=permonitorv2` before
SDL starts. Without it Windows lies to the process about the size of the desktop: on
a display scaled to 150% it reports 1280x720 for a 1920x1080 panel and then stretches
everything drawn. The overlay would cover two thirds of the screen and the blob would
be soft.

It also matters for correctness rather than just looks: `GetCursorPos` returns
physical pixels, and the click-through test compares it against rectangles in the
window's coordinates. Those two only agree when the process is DPI aware.

## Settled deliberately

**`WS_EX_NOACTIVATE` is set, so `Esc` does not work.** Clicking the blob does not
take focus from whatever you were working in — which is the right behaviour for a
desk toy, and is the one thing the macOS build *cannot* do. The cost is that the
overlay never receives keyboard focus, so the `Esc` binding never fires on Windows.
Middle-click on the blob and `Ctrl+C` in the console both still quit.

**The window is a tool window.** `WS_EX_TOOLWINDOW` keeps it out of the taskbar and
out of Alt+Tab, which is the Win32 spelling of the `_NET_WM_STATE_SKIP_TASKBAR` and
`_SKIP_PAGER` hints the X11 build sets.

## Throwing a blob to the other machines

Nothing platform-specific:

```powershell
cargo net
```

Windows Defender Firewall will ask, the first time, whether `liquid-metal` may accept
incoming connections. It has to be allowed on the network profile you are actually on
— tick **Private networks** for a home or office LAN. Refuse it and discovery still
appears to work, because beacons are UDP and go out fine, but every throw lands
nowhere and the blob bounces back off the edge it left by.

If discovery is blocked and you would rather not fight it, name the peer directly
using the port from the other machine's startup log:

```powershell
cargo net --peer 192.168.1.72:41521 --no-discovery
```

## If something is wrong on the first run

**A black or grey rectangle covering the screen.** DWM did not put the window on the
per-pixel alpha path. Check the `DWM alpha` line in the startup log for the HRESULT.
If it reports success and the screen is still opaque, try `WS_EX_LAYERED` as
described above.

**Clicks do not reach the desktop.** The click-through toggle is not tracking the
cursor. Confirm with `--windowed` that the blob and physics are fine, then check that
`GetCursorPos` and `SDL_GetDisplayBounds` agree about coordinates — the whole scheme
rests on those being in the same space, which is what the DPI awareness hint buys.

**The overlay covers only part of the screen, or the blob is blurry.** DPI awareness
did not take. The hint has to be set before `SDL_Init`; check that
`prepare_process_environment` is still being called before the SDL video subsystem
comes up.

## Known gaps

- **Displays plugged in or unplugged while running are not picked up.** The desktop
  is measured once, at startup. Restart to pick up a new arrangement. Same as macOS.
- **Nothing here has run.** See above.
