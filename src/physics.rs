//! Soft-body blob simulation.
//!
//! Deliberately free of every SDL / GL / X11 type: this is the only part of the
//! program that can be exercised headlessly, so it has to stay that way. It does
//! name `wire::Edge`, which is pure data and just as headless — better one shared
//! enum than two identical ones and a conversion between them.
//!
//! Model: one heavy `core` particle plus `SAT_COUNT` satellites, each spring-attached
//! to a rest offset on a circle around the core. Rendering unions all of them as a
//! single metaball field, so the satellites lagging behind the core *is* the stretch,
//! and their spring ring-down *is* the wobble. Neither is faked separately.

use std::f32::consts::TAU;

use crate::wire::Edge;

// ---------------------------------------------------------------------------
// TUNABLES — everything you would touch to change how the blob feels lives here.
// ---------------------------------------------------------------------------

/// Number of satellite particles around the core.
pub const SAT_COUNT: usize = 8;

/// Metaball radius of the core particle, in pixels.
pub const CORE_RADIUS: f32 = 36.0;
/// Metaball radius of each satellite.
pub const SAT_RADIUS: f32 = 24.0;
/// Rest distance of a satellite from the core.
pub const SAT_ORBIT: f32 = 22.0;
/// Field strength multiplier for the core / satellites (shapes the union).
pub const CORE_STRENGTH: f32 = 1.0;
pub const SAT_STRENGTH: f32 = 0.85;

/// Radius used for wall collision. This is *measured*, not guessed: the metaball
/// union extends far past any individual ball, so `iso_radius()` below finds where
/// the rendered `field == 1` surface actually is, and `collide_radius_matches_the_
/// rendered_surface` keeps this constant honest if you retune the radii.
pub const COLLIDE_RADIUS: f32 = 75.0;

/// Satellite spring stiffness (1/s^2) and damping (1/s), damped against the *core's*
/// velocity so a constant-velocity glide settles instead of trailing forever.
pub const SPRING_K: f32 = 180.0;
pub const SPRING_DAMP: f32 = 12.0;

/// Hard cap on how far a satellite may stray from the core, in pixels. Purely a
/// stability net: the worst the scripted self-test provokes is ~49 px, on the whip
/// at release, so there is real headroom before the clamp would ever show.
pub const MAX_SAT_DIST: f32 = 70.0;

/// Drag spring used while the pointer holds the blob. Stiff, but not a teleport —
/// the lag between cursor and core is most of the character.
pub const GRAB_K: f32 = 260.0;
pub const GRAB_DAMP: f32 = 28.0;

/// Free-flight damping. `v *= exp(-DRAG_K * dt)` gives the long coast; the small
/// constant `FRICTION` is what actually brings it to a stop in finite time.
pub const DRAG_K: f32 = 0.6;
pub const FRICTION: f32 = 70.0;

/// Below this speed (px/s) a free blob is simply stopped, and considered at rest.
pub const REST_SPEED: f32 = 6.0;

/// Wall bounce restitution, and how hard an impact squashes the satellites.
pub const RESTITUTION: f32 = 0.65;
pub const IMPACT_KICK: f32 = 0.18;
/// Ceiling on the excursion, in pixels, that a wall impact may push a satellite to.
///
/// A velocity kick `v` on a spring of stiffness `k` produces a peak displacement of
/// `v / sqrt(k)`, so an unbounded kick tears the blob into separate lumps on a hard
/// fling. Capping the *displacement* and deriving the velocity from it keeps the
/// ripple proportional at ordinary speeds and bounded at silly ones — and it stays
/// correct if you retune SPRING_K.
pub const IMPACT_RIPPLE_PX: f32 = 16.0;

/// Squash & stretch: elongation along the velocity, area preserved by shrinking
/// perpendicular. `STRETCH_PER_SPEED` px/s maps speed to elongation; capped at
/// `MAX_STRETCH` so a hard fling stays a blob and does not become a needle.
pub const MAX_STRETCH: f32 = 1.6;
pub const STRETCH_PER_SPEED: f32 = 1.0 / 2200.0;

/// Window, in seconds, over which release velocity is averaged. Using only the last
/// frame's delta gives noisy, feeble throws — this window is most of the good feel.
pub const THROW_WINDOW: f64 = 0.080;
/// Cap on the estimated throw speed, px/s.
pub const MAX_THROW_SPEED: f32 = 6000.0;

/// Fixed simulation substep. 240 Hz, independent of the render rate.
pub const SUBSTEP: f32 = 1.0 / 240.0;
/// Never let the accumulator ask for more than this much simulated time in one frame,
/// so a stall (or a laptop resuming from sleep) cannot spiral.
pub const MAX_FRAME_TIME: f32 = 0.25;

// --- leaving the screen for another machine ---
/// Slowest a blob may be travelling as it arrives from a peer, in px/s.
///
/// A throw arrives just outside the edge and has to coast *in*. Too slow and it
/// hangs off the side of the screen; a peer sending an outward or near-zero
/// velocity — buggy, or just a very gentle throw across mismatched screens — would
/// otherwise leave the blob stranded where nobody can grab it.
pub const MIN_ENTRY_SPEED: f32 = 150.0;

/// How far outside the edge an arriving blob starts, in units of `COLLIDE_RADIUS`.
/// Far enough that it visibly slides on rather than popping into existence.
pub const ENTRY_MARGIN: f32 = 1.15;

// --- input-region rasterisation ---
/// Cell size in pixels of the coarse grid used to build the XShape input region.
pub const HIT_CELL: i32 = 14;
/// Field threshold for that grid. Lower than the rendered isosurface (1.0), so the
/// grabbable area already sits slightly outside the visible metal.
pub const HIT_ISO: f32 = 0.70;
/// Extra dilation of the coarse mask, in cells. One cell of dilation plus the
/// quantisation gives ~14-28 px of comfortable grab margin.
pub const HIT_DILATE: i32 = 1;

// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

pub const fn v2(x: f32, y: f32) -> Vec2 {
    Vec2 { x, y }
}

impl Vec2 {
    pub fn len(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
    pub fn len_sq(self) -> f32 {
        self.x * self.x + self.y * self.y
    }
    pub fn dot(self, o: Vec2) -> f32 {
        self.x * o.x + self.y * o.y
    }
    /// Unit vector, or `(0,0)` for a degenerate input (never a NaN).
    pub fn norm_or_zero(self) -> Vec2 {
        let l = self.len();
        if l > 1e-6 { self * (1.0 / l) } else { v2(0.0, 0.0) }
    }
    pub fn perp(self) -> Vec2 {
        v2(-self.y, self.x)
    }
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl std::ops::Add for Vec2 {
    type Output = Vec2;
    fn add(self, o: Vec2) -> Vec2 {
        v2(self.x + o.x, self.y + o.y)
    }
}
impl std::ops::Sub for Vec2 {
    type Output = Vec2;
    fn sub(self, o: Vec2) -> Vec2 {
        v2(self.x - o.x, self.y - o.y)
    }
}
impl std::ops::Mul<f32> for Vec2 {
    type Output = Vec2;
    fn mul(self, s: f32) -> Vec2 {
        v2(self.x * s, self.y * s)
    }
}
impl std::ops::Neg for Vec2 {
    type Output = Vec2;
    fn neg(self) -> Vec2 {
        v2(-self.x, -self.y)
    }
}
impl std::ops::AddAssign for Vec2 {
    fn add_assign(&mut self, o: Vec2) {
        *self = *self + o;
    }
}
impl std::ops::SubAssign for Vec2 {
    fn sub_assign(&mut self, o: Vec2) {
        *self = *self - o;
    }
}

/// Axis-aligned rectangle in screen pixels, `x0..x1` / `y0..y1`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Bounds {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Bounds {
    pub fn screen(w: f32, h: f32) -> Bounds {
        Bounds { x0: 0.0, y0: 0.0, x1: w, y1: h }
    }
    pub fn center(&self) -> Vec2 {
        v2((self.x0 + self.x1) * 0.5, (self.y0 + self.y1) * 0.5)
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct Particle {
    pub p: Vec2,
    pub v: Vec2,
}

/// One metaball as handed to the shader: position, radius, field strength.
#[derive(Copy, Clone, Debug, Default)]
pub struct Ball {
    pub p: Vec2,
    pub r: f32,
    pub w: f32,
}

/// Timestamped pointer sample ring, used to estimate release velocity.
#[derive(Clone, Debug, Default)]
pub struct PointerTrack {
    samples: std::collections::VecDeque<(f64, Vec2)>,
}

impl PointerTrack {
    pub fn new() -> Self {
        PointerTrack { samples: std::collections::VecDeque::with_capacity(64) }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Record where the pointer was at time `t` (seconds, monotonic).
    pub fn push(&mut self, t: f64, p: Vec2) {
        self.samples.push_back((t, p));
        // Keep a little more than the estimation window so there is always one
        // sample older than `now - THROW_WINDOW` to interpolate from.
        while self.samples.len() > 2 {
            let oldest = self.samples[0].0;
            if t - oldest > THROW_WINDOW * 3.0 {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Average pointer velocity over the last `THROW_WINDOW` seconds, in px/s.
    ///
    /// This is displacement over elapsed time across the window rather than the last
    /// frame's delta, which is what makes a fling land where you expect.
    pub fn velocity(&self, now: f64) -> Vec2 {
        if self.samples.len() < 2 {
            return v2(0.0, 0.0);
        }
        let cutoff = now - THROW_WINDOW;
        // Oldest sample still inside the window; if every sample is older, fall back
        // to the second-newest so a slow pointer still yields something sensible.
        let mut first = self.samples.len() - 2;
        for (i, s) in self.samples.iter().enumerate() {
            if s.0 >= cutoff {
                first = i;
                break;
            }
        }
        let (t0, p0) = self.samples[first];
        let (t1, p1) = self.samples[self.samples.len() - 1];
        let dt = t1 - t0;
        if dt <= 1e-4 {
            return v2(0.0, 0.0);
        }
        let v = (p1 - p0) * (1.0 / dt as f32);
        let speed = v.len();
        if speed > MAX_THROW_SPEED { v * (MAX_THROW_SPEED / speed) } else { v }
    }
}

#[derive(Clone, Debug)]
pub struct Blob {
    pub core: Particle,
    pub sats: [Particle; SAT_COUNT],
    /// Rest offsets on the unit circle scaled to `SAT_ORBIT`, in blob-local space.
    rest: [Vec2; SAT_COUNT],
    pub bounds: Bounds,
    /// Rectangles inside `bounds` that no display covers, which the blob is kept out
    /// of. A desktop is a bounding box, not a shape: displays of different heights
    /// leave holes under the short ones belonging to no screen at all, and a blob
    /// flung into one simply disappears. Empty on a single display, and on any
    /// desktop whose displays happen to tile the box exactly.
    dead: Vec<Bounds>,
    /// `Some(offset)` while held: the vector from core to the point that was grabbed,
    /// so the blob does not snap its centre to the cursor.
    grab: Option<Vec2>,
    grab_target: Vec2,
    /// Seconds since the blob last moved meaningfully; drives the idle throttle.
    pub still_for: f32,
    /// Bitmask of `Edge`s that lead to another machine. Those stop being walls: the
    /// blob passes straight through and the frame loop turns that into a throw.
    portals: u8,
    /// True while the blob is outside its own bounds *on its way in* — freshly
    /// arrived from a peer, or bounced back after a throw that did not connect.
    ///
    /// One flag for both cases, because they are the same situation: something is
    /// out there heading inward and must not be mistaken for something leaving.
    /// Cleared by `step` the moment the core is properly inside.
    entering: bool,
}

impl Blob {
    pub fn new(bounds: Bounds) -> Blob {
        let mut rest = [Vec2::default(); SAT_COUNT];
        for (i, r) in rest.iter_mut().enumerate() {
            let a = TAU * (i as f32) / (SAT_COUNT as f32);
            *r = v2(a.cos(), a.sin()) * SAT_ORBIT;
        }
        let c = bounds.center();
        let mut b = Blob {
            core: Particle { p: c, v: v2(0.0, 0.0) },
            sats: [Particle::default(); SAT_COUNT],
            rest,
            bounds,
            dead: Vec::new(),
            grab: None,
            grab_target: c,
            still_for: 0.0,
            portals: 0,
            entering: false,
        };
        b.snap_satellites();
        b
    }

    /// A blob arriving from another machine, placed just outside `edge` and moving
    /// inward, carrying the deformation it was thrown with.
    ///
    /// `along` is the 0..=1 position along that edge, `vel` is already in this
    /// machine's pixels per second, and `sats` are offsets and relative velocities
    /// in units of `SAT_ORBIT` — see `wire.rs` for why those are the units.
    pub fn arriving(
        bounds: Bounds,
        edge: Edge,
        along: f32,
        vel: Vec2,
        sats: &[crate::wire::SatState],
    ) -> Blob {
        let mut b = Blob::new(bounds);
        let along = along.clamp(0.0, 1.0);
        let m = COLLIDE_RADIUS * ENTRY_MARGIN;
        let (w, h) = (bounds.x1 - bounds.x0, bounds.y1 - bounds.y0);
        b.core.p = match edge {
            Edge::Left => v2(bounds.x0 - m, bounds.y0 + along * h),
            Edge::Right => v2(bounds.x1 + m, bounds.y0 + along * h),
            Edge::Top => v2(bounds.x0 + along * w, bounds.y0 - m),
            Edge::Bottom => v2(bounds.x0 + along * w, bounds.y1 + m),
        };

        // The inward normal of the edge it is coming through.
        let n = match edge {
            Edge::Left => v2(1.0, 0.0),
            Edge::Right => v2(-1.0, 0.0),
            Edge::Top => v2(0.0, 1.0),
            Edge::Bottom => v2(0.0, -1.0),
        };
        // Whatever the peer sent, it has to be heading onto this screen fast enough
        // to get here. Anything else strands the blob off the edge.
        let mut v = if vel.is_finite() { vel } else { v2(0.0, 0.0) };
        let inward = v.dot(n);
        if inward < 0.0 {
            v -= n * (2.0 * inward); // reflect, keeping the tangential component
        }
        let inward = v.dot(n);
        if inward < MIN_ENTRY_SPEED {
            v += n * (MIN_ENTRY_SPEED - inward);
        }
        b.core.v = v;

        b.snap_satellites();
        // Re-apply the thrown shape on top of the rest pose. A peer that models its
        // blob differently sends a different count (or none); then the blob simply
        // arrives round, which is a worse throw but still a throw.
        if sats.len() == SAT_COUNT {
            for (i, s) in sats.iter().enumerate() {
                let off = v2(s.off_x, s.off_y) * SAT_ORBIT;
                let rel = v2(s.vel_x, s.vel_y) * SAT_ORBIT;
                if off.is_finite() && rel.is_finite() && off.len() <= MAX_SAT_DIST {
                    b.sats[i].p = b.core.p + off;
                    b.sats[i].v = b.core.v + rel;
                }
            }
        }
        b.entering = true;
        b
    }

    /// Put every satellite exactly at its rest offset with zero relative velocity.
    ///
    /// The rest offset already accounts for squash & stretch, so this settles the
    /// blob into the shape it would hold at its current velocity.
    pub fn snap_satellites(&mut self) {
        for i in 0..SAT_COUNT {
            self.sats[i].p = self.rest_world(i);
            self.sats[i].v = self.core.v;
        }
    }

    /// Recentre and stop dead. Used by the double-click reset.
    pub fn reset(&mut self) {
        self.core.p = self.bounds.center();
        self.core.v = v2(0.0, 0.0);
        self.grab = None;
        self.grab_target = self.core.p;
        self.still_for = 0.0;
        self.entering = false;
        self.snap_satellites();
    }

    pub fn set_bounds(&mut self, b: Bounds) {
        self.bounds = b;
    }

    /// The holes in the desktop, in the same window-relative pixels as `bounds`.
    ///
    /// Recomputed with the bounds whenever the window moves or the displays change,
    /// because a hole is defined by where the displays are, not by the blob.
    pub fn set_dead_regions(&mut self, rects: &[Bounds]) {
        self.dead.clear();
        self.dead.extend_from_slice(rects);
    }

    /// Set which edges are doors rather than walls.
    pub fn set_portals(&mut self, mask: u8) {
        self.portals = mask;
    }

    /// Read only by the tests — which is the point of it. Asserting on the mask is
    /// how they check that a door was opened where one was asked for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn portals(&self) -> u8 {
        self.portals
    }

    fn portal_open(&self, e: Edge) -> bool {
        self.portals & e.bit() != 0
    }

    /// True while the blob is outside the screen heading in. Also test-facing: the
    /// arrival tests need to see the flag clear on its own once the blob is in.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_entering(&self) -> bool {
        self.entering
    }

    /// The portal edge this blob has just left through, if any.
    ///
    /// The test is the *same line* an ordinary wall bounce happens at, so a portal
    /// is exactly a bounce that was allowed to continue. A blob on its way in is
    /// never leaving, however far outside it currently is — that is what
    /// `entering` is for.
    pub fn departing_edge(&self) -> Option<Edge> {
        if self.entering || self.grab.is_some() {
            return None;
        }
        let r = COLLIDE_RADIUS;
        let b = self.bounds;
        let (p, v) = (self.core.p, self.core.v);
        let candidates = [
            (Edge::Left, p.x < b.x0 + r, v.x < 0.0),
            (Edge::Right, p.x > b.x1 - r, v.x > 0.0),
            (Edge::Top, p.y < b.y0 + r, v.y < 0.0),
            (Edge::Bottom, p.y > b.y1 - r, v.y > 0.0),
        ];
        candidates
            .iter()
            .find(|(e, past, outward)| *past && *outward && self.portal_open(*e))
            .map(|(e, _, _)| *e)
    }

    /// Where along `edge` the core is, as a 0..=1 fraction. This is what crosses
    /// the wire, so the blob turns up at the same height on a screen of a different
    /// size.
    pub fn along_edge(&self, edge: Edge) -> f32 {
        let b = self.bounds;
        let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
        let f = match edge {
            Edge::Left | Edge::Right => {
                if h > 1e-3 { (self.core.p.y - b.y0) / h } else { 0.5 }
            }
            Edge::Top | Edge::Bottom => {
                if w > 1e-3 { (self.core.p.x - b.x0) / w } else { 0.5 }
            }
        };
        f.clamp(0.0, 1.0)
    }

    /// Satellite offsets and velocities relative to the core, in units of
    /// `SAT_ORBIT`, ready for the wire.
    pub fn sat_states(&self) -> Vec<crate::wire::SatState> {
        let inv = 1.0 / SAT_ORBIT;
        self.sats
            .iter()
            .map(|s| {
                let off = (s.p - self.core.p) * inv;
                let rel = (s.v - self.core.v) * inv;
                crate::wire::SatState {
                    off_x: off.x,
                    off_y: off.y,
                    vel_x: rel.x,
                    vel_y: rel.y,
                }
            })
            .collect()
    }

    /// Turn a departure back into a bounce, because the throw did not connect.
    ///
    /// The blob is outside the screen by now, so it is not snapped back to the wall
    /// — that would be a visible teleport. Its outward velocity is reversed and it
    /// is marked as entering, so it coasts back on under its own power and reads as
    /// a bounce that took a moment to happen.
    pub fn bounce_back(&mut self, edge: Edge) {
        let n = match edge {
            Edge::Left => v2(1.0, 0.0),
            Edge::Right => v2(-1.0, 0.0),
            Edge::Top => v2(0.0, 1.0),
            Edge::Bottom => v2(0.0, -1.0),
        };
        let inward = self.core.v.dot(n);
        if inward < 0.0 {
            self.core.v -= n * (inward * (1.0 + RESTITUTION));
        }
        let inward = self.core.v.dot(n);
        if inward < MIN_ENTRY_SPEED {
            self.core.v += n * (MIN_ENTRY_SPEED - inward);
        }
        self.entering = true;
        self.still_for = 0.0;
    }

    /// True once the blob is far enough outside that none of it is on screen. The
    /// frame loop stops drawing it and stops stepping it at this point, so a throw
    /// still waiting for its receipt does not coast off to infinity.
    pub fn is_off_screen(&self) -> bool {
        let bb = self.bbox();
        let b = self.bounds;
        bb.x1 < b.x0 || bb.x0 > b.x1 || bb.y1 < b.y0 || bb.y0 > b.y1
    }

    pub fn is_grabbed(&self) -> bool {
        self.grab.is_some()
    }

    /// Begin a drag. `at` is where the pointer went down, in screen pixels.
    pub fn grab(&mut self, at: Vec2) {
        self.grab = Some(at - self.core.p);
        self.grab_target = at;
        self.still_for = 0.0;
    }

    /// Move the drag target. Cheap; call it from every motion event.
    pub fn drag_to(&mut self, at: Vec2) {
        if self.grab.is_some() {
            self.grab_target = at;
            self.still_for = 0.0;
        }
    }

    /// End the drag, handing the blob the estimated throw velocity.
    pub fn release(&mut self, throw: Vec2) {
        self.grab = None;
        self.core.v = throw;
        self.still_for = 0.0;
    }

    /// Elongation factor along the velocity direction (>= 1). Perpendicular gets
    /// `1/stretch`, which preserves area.
    pub fn stretch(&self) -> f32 {
        let s = 1.0 + self.core.v.len() * STRETCH_PER_SPEED;
        s.min(MAX_STRETCH)
    }

    /// World-space rest position of satellite `i`, after squash & stretch.
    fn rest_world(&self, i: usize) -> Vec2 {
        let s = self.stretch();
        let dir = self.core.v.norm_or_zero();
        let off = self.rest[i];
        if dir.len_sq() < 1e-9 {
            return self.core.p + off;
        }
        let perp = dir.perp();
        let along = off.dot(dir) * s;
        let across = off.dot(perp) / s;
        self.core.p + dir * along + perp * across
    }

    /// Advance one fixed substep.
    pub fn step(&mut self, dt: f32) {
        // --- core ---
        if let Some(off) = self.grab {
            let target = self.grab_target - off;
            let a = (target - self.core.p) * GRAB_K - self.core.v * GRAB_DAMP;
            self.core.v += a * dt;
        } else {
            // Exponential decay for the long coast...
            self.core.v = self.core.v * (-DRAG_K * dt).exp();
            // ...plus a small constant deceleration so it actually stops.
            let speed = self.core.v.len();
            if speed > 0.0 {
                let drop = (FRICTION * dt).min(speed);
                self.core.v -= self.core.v.norm_or_zero() * drop;
            }
            if self.core.v.len() < REST_SPEED {
                self.core.v = v2(0.0, 0.0);
            }
        }
        self.core.p += self.core.v * dt;

        // Once a blob that was on its way in is properly inside, it is an ordinary
        // blob again — and only then may it be considered to be leaving. Using the
        // bounce line rather than the screen edge means it has to be clear of the
        // door before the door can be used again, so an arrival cannot bounce
        // straight back out the way it came.
        if self.entering {
            let r = COLLIDE_RADIUS;
            let b = self.bounds;
            let p = self.core.p;
            if p.x >= b.x0 + r && p.x <= b.x1 - r && p.y >= b.y0 + r && p.y <= b.y1 - r {
                self.entering = false;
            }
        }

        self.collide_walls();

        // --- satellites ---
        for i in 0..SAT_COUNT {
            let target = self.rest_world(i);
            let s = &mut self.sats[i];
            // Damped against the core's velocity: lag comes from acceleration, not
            // from steady motion.
            let a = (target - s.p) * SPRING_K - (s.v - self.core.v) * SPRING_DAMP;
            s.v += a * dt;
            s.p += s.v * dt;

            // Stability net.
            let off = s.p - self.core.p;
            let d = off.len();
            if d > MAX_SAT_DIST {
                s.p = self.core.p + off * (MAX_SAT_DIST / d);
                s.v = self.core.v;
            }
        }

        // --- idle tracking ---
        let mut max_rel = 0.0f32;
        for s in &self.sats {
            max_rel = max_rel.max((s.v - self.core.v).len());
        }
        if self.grab.is_none() && self.core.v.len_sq() == 0.0 && max_rel < REST_SPEED {
            self.still_for += dt;
        } else {
            self.still_for = 0.0;
        }
    }

    /// True once the blob has been motionless long enough to throttle the frame rate.
    pub fn is_at_rest(&self) -> bool {
        self.grab.is_none() && self.still_for > 0.25
    }

    fn collide_walls(&mut self) {
        let r = COLLIDE_RADIUS;
        let (lo_x, hi_x) = (self.bounds.x0 + r, self.bounds.x1 - r);
        let (lo_y, hi_y) = (self.bounds.y0 + r, self.bounds.y1 - r);

        // Degenerate screens (narrower than the blob) would otherwise ping-pong.
        if hi_x <= lo_x || hi_y <= lo_y {
            self.core.p = self.bounds.center();
            self.entering = false;
            return;
        }

        // An edge that leads to another machine is not a wall. The blob sails
        // straight through the line it would have bounced off, and `departing_edge`
        // — which tests that same line — turns that into a throw.
        let mut hit: Option<(Vec2, f32)> = None;
        if self.core.p.x < lo_x && !self.portal_open(Edge::Left) {
            let impact = (-self.core.v.x).max(0.0);
            self.core.p.x = lo_x;
            self.core.v.x = self.core.v.x.abs() * RESTITUTION;
            hit = Some((v2(1.0, 0.0), impact));
        } else if self.core.p.x > hi_x && !self.portal_open(Edge::Right) {
            let impact = self.core.v.x.max(0.0);
            self.core.p.x = hi_x;
            self.core.v.x = -self.core.v.x.abs() * RESTITUTION;
            hit = Some((v2(-1.0, 0.0), impact));
        }
        if self.core.p.y < lo_y && !self.portal_open(Edge::Top) {
            let impact = (-self.core.v.y).max(0.0);
            self.core.p.y = lo_y;
            self.core.v.y = self.core.v.y.abs() * RESTITUTION;
            hit = Some((v2(0.0, 1.0), impact));
        } else if self.core.p.y > hi_y && !self.portal_open(Edge::Bottom) {
            let impact = self.core.v.y.max(0.0);
            self.core.p.y = hi_y;
            self.core.v.y = -self.core.v.y.abs() * RESTITUTION;
            hit = Some((v2(0.0, -1.0), impact));
        }

        // The holes. Each is an obstacle grown by the blob's radius, so the body
        // stops at the edge of the missing screen rather than half-vanishing into
        // it. Indexed rather than iterated because the core is mutated inside.
        for i in 0..self.dead.len() {
            let d = self.dead[i];
            let (x0, y0, x1, y1) = (d.x0 - r, d.y0 - r, d.x1 + r, d.y1 + r);
            let p = self.core.p;
            if p.x <= x0 || p.x >= x1 || p.y <= y0 || p.y >= y1 {
                continue;
            }
            // Leave by the nearest side: anything else teleports the blob across the
            // hole when it enters near a corner.
            let (dl, dr_, dt, db) = (p.x - x0, x1 - p.x, p.y - y0, y1 - p.y);
            let m = dl.min(dr_).min(dt).min(db);
            if m == dl {
                let impact = self.core.v.x.max(0.0);
                self.core.p.x = x0;
                self.core.v.x = -self.core.v.x.abs() * RESTITUTION;
                hit = Some((v2(-1.0, 0.0), impact));
            } else if m == dr_ {
                let impact = (-self.core.v.x).max(0.0);
                self.core.p.x = x1;
                self.core.v.x = self.core.v.x.abs() * RESTITUTION;
                hit = Some((v2(1.0, 0.0), impact));
            } else if m == dt {
                let impact = self.core.v.y.max(0.0);
                self.core.p.y = y0;
                self.core.v.y = -self.core.v.y.abs() * RESTITUTION;
                hit = Some((v2(0.0, -1.0), impact));
            } else {
                let impact = (-self.core.v.y).max(0.0);
                self.core.p.y = y1;
                self.core.v.y = self.core.v.y.abs() * RESTITUTION;
                hit = Some((v2(0.0, 1.0), impact));
            }
        }

        if let Some((n, impact)) = hit {
            if impact > REST_SPEED {
                let kick = (impact * IMPACT_KICK).min(IMPACT_RIPPLE_PX * SPRING_K.sqrt());
                for i in 0..SAT_COUNT {
                    // The whole body hits the wall, not just the core. Without
                    // reflecting the satellites too, the core reverses while they
                    // keep ploughing into the wall at full speed — a relative
                    // velocity of `(1 + RESTITUTION) * impact`, which on a spring of
                    // stiffness k throws them `v / sqrt(k)` out and tears the blob
                    // into separate lumps on any hard fling.
                    let vn = self.sats[i].v.dot(n);
                    if vn < 0.0 {
                        self.sats[i].v -= n * (vn * (1.0 + RESTITUTION));
                    }
                    // What is left is the deliberate part: squash the far-side
                    // satellites toward the wall so the impact visibly ripples
                    // through the body instead of the whole thing just reversing.
                    let off_dir = (self.sats[i].p - self.core.p).norm_or_zero();
                    let w = off_dir.dot(n).max(0.0);
                    self.sats[i].v -= n * (kick * w);
                }
            }
        }
    }

    /// Metaballs for the renderer: core first, then satellites.
    pub fn balls(&self) -> [Ball; SAT_COUNT + 1] {
        let mut out = [Ball::default(); SAT_COUNT + 1];
        out[0] = Ball { p: self.core.p, r: CORE_RADIUS, w: CORE_STRENGTH };
        for i in 0..SAT_COUNT {
            out[i + 1] = Ball { p: self.sats[i].p, r: SAT_RADIUS, w: SAT_STRENGTH };
        }
        out
    }

    /// The scalar field the shader draws, `sum(w * r^2 / (|d|^2 + eps))`.
    /// Kept in sync with `shader.frag` by construction; the isosurface is `f == 1`.
    pub fn field(&self, at: Vec2) -> f32 {
        let mut f = 0.0;
        for b in self.balls() {
            let d = at - b.p;
            f += b.w * b.r * b.r / (d.len_sq() + 1.0);
        }
        f
    }

    /// Bounding box of the rendered blob, generously padded.
    pub fn bbox(&self) -> Bounds {
        let balls = self.balls();
        let mut bb = Bounds {
            x0: f32::INFINITY,
            y0: f32::INFINITY,
            x1: f32::NEG_INFINITY,
            y1: f32::NEG_INFINITY,
        };
        // Far from the blob the field behaves like a single ball carrying all the
        // weight, so padding every ball by that radius is a safe over-estimate of
        // where `field == HIT_ISO` can possibly reach.
        let total_wr2: f32 = balls.iter().map(|b| b.w * b.r * b.r).sum();
        let pad = (total_wr2 / HIT_ISO).sqrt() + 4.0;
        for b in balls {
            bb.x0 = bb.x0.min(b.p.x - pad);
            bb.y0 = bb.y0.min(b.p.y - pad);
            bb.x1 = bb.x1.max(b.p.x + pad);
            bb.y1 = bb.y1.max(b.p.y + pad);
        }
        bb
    }

    /// Coarse rectangle cover of the blob for the XShape input region.
    ///
    /// Rasterises the field onto a `HIT_CELL` grid over the bounding box, thresholds
    /// at `HIT_ISO`, dilates by `HIT_DILATE` cells, then merges horizontal runs. The
    /// result is a dozen or two rects that sit slightly outside the visible metal.
    ///
    /// Coordinates are relative to the window origin, which for the overlay is (0,0)
    /// of the X virtual screen.
    pub fn hit_rects(&self) -> Vec<(i32, i32, i32, i32)> {
        let bb = self.bbox();
        let c = HIT_CELL;
        let gx0 = (bb.x0 / c as f32).floor() as i32 - HIT_DILATE;
        let gy0 = (bb.y0 / c as f32).floor() as i32 - HIT_DILATE;
        let gx1 = (bb.x1 / c as f32).ceil() as i32 + HIT_DILATE;
        let gy1 = (bb.y1 / c as f32).ceil() as i32 + HIT_DILATE;
        let w = (gx1 - gx0).max(0) as usize;
        let h = (gy1 - gy0).max(0) as usize;
        if w == 0 || h == 0 || w > 4096 || h > 4096 {
            return Vec::new();
        }

        let mut mask = vec![false; w * h];
        for gy in 0..h {
            for gx in 0..w {
                let px = ((gx0 + gx as i32) as f32 + 0.5) * c as f32;
                let py = ((gy0 + gy as i32) as f32 + 0.5) * c as f32;
                if self.field(v2(px, py)) >= HIT_ISO {
                    mask[gy * w + gx] = true;
                }
            }
        }

        // Dilate so the grabbable area is comfortably larger than the metal.
        let mut grown = mask.clone();
        for _ in 0..HIT_DILATE {
            let src = grown.clone();
            for gy in 0..h {
                for gx in 0..w {
                    if src[gy * w + gx] {
                        continue;
                    }
                    let mut on = false;
                    for dy in -1i32..=1 {
                        for dx in -1i32..=1 {
                            let nx = gx as i32 + dx;
                            let ny = gy as i32 + dy;
                            if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                                on |= src[ny as usize * w + nx as usize];
                            }
                        }
                    }
                    grown[gy * w + gx] = on;
                }
            }
        }

        // Merge horizontal runs into one rect each.
        let mut rects = Vec::new();
        for gy in 0..h {
            let mut gx = 0usize;
            while gx < w {
                if !grown[gy * w + gx] {
                    gx += 1;
                    continue;
                }
                let start = gx;
                while gx < w && grown[gy * w + gx] {
                    gx += 1;
                }
                let x = (gx0 + start as i32) * c;
                let y = (gy0 + gy as i32) * c;
                let rw = (gx - start) as i32 * c;
                rects.push((x, y, rw, c));
            }
        }
        rects
    }

    /// Whether a point falls inside the same rectangle cover that becomes the XShape
    /// input region. Used only by `--windowed`, where no input region exists; the
    /// overlay never second-guesses X's hit test with this.
    pub fn hit_test(&self, at: Vec2) -> bool {
        let x = at.x.floor() as i32;
        let y = at.y.floor() as i32;
        self.hit_rects()
            .iter()
            .any(|&(rx, ry, rw, rh)| x >= rx && x < rx + rw && y >= ry && y < ry + rh)
    }

    /// Distance from the core at which the rendered `field` crosses `iso`, measured
    /// by bisection along +x with the blob in its rest pose. Used to keep
    /// `COLLIDE_RADIUS` and the grab margin in step with the radius tunables.
    pub fn iso_radius(&self, iso: f32) -> f32 {
        let (mut lo, mut hi) = (1.0f32, 4000.0f32);
        for _ in 0..60 {
            let mid = 0.5 * (lo + hi);
            if self.field(self.core.p + v2(mid, 0.0)) > iso {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Every finite check the self-test cares about, in one place.
    pub fn all_finite(&self) -> bool {
        self.core.p.is_finite()
            && self.core.v.is_finite()
            && self.sats.iter().all(|s| s.p.is_finite() && s.v.is_finite())
    }

    pub fn max_sat_dist(&self) -> f32 {
        self.sats
            .iter()
            .map(|s| (s.p - self.core.p).len())
            .fold(0.0f32, f32::max)
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The desktop is a bounding box; the blob must not fall into the parts of it
    /// that are not a screen.
    ///
    /// Modelled on a real three-display desk: 2560x1440, 2560x1440 and 1512x982 in a
    /// row make a 6632x1440 desktop with a 1512x458 hole under the short one.
    #[test]
    fn the_blob_is_kept_out_of_the_hole_between_displays() {
        let world = Bounds::screen(6632.0, 1440.0);
        let hole = Bounds { x0: 5120.0, y0: 982.0, x1: 6632.0, y1: 1440.0 };
        let mut b = Blob::new(world);
        b.set_dead_regions(&[hole]);

        // Aimed down into the hole from above the short display.
        b.core.p = v2(5800.0, 900.0);
        b.core.v = v2(0.0, 1200.0);
        for _ in 0..480 {
            b.step(SUBSTEP);
        }

        assert!(
            b.core.p.y <= hole.y0 - COLLIDE_RADIUS + 1.0,
            "the blob sank into the hole: y = {}, hole starts at {}",
            b.core.p.y,
            hole.y0
        );
        // And it is still on the desktop, not flung out of the far side of the hole.
        assert!(b.core.p.x > world.x0 && b.core.p.x < world.x1);
    }

    /// The hole is not a wall for a screen that does reach that far down: the blob
    /// must still be able to use the full height of the tall displays.
    #[test]
    fn the_hole_does_not_wall_off_the_taller_displays() {
        let world = Bounds::screen(6632.0, 1440.0);
        let hole = Bounds { x0: 5120.0, y0: 982.0, x1: 6632.0, y1: 1440.0 };
        let mut b = Blob::new(world);
        b.set_dead_regions(&[hole]);

        b.core.p = v2(2000.0, 900.0);
        b.core.v = v2(0.0, 1200.0);
        for _ in 0..480 {
            b.step(SUBSTEP);
        }

        assert!(
            b.core.p.y > 982.0,
            "the blob stopped at the short display's height on a tall one: y = {}",
            b.core.p.y
        );
    }

    fn screen() -> Bounds {
        Bounds::screen(1920.0, 1080.0)
    }

    #[test]
    fn spring_rest_state_is_stable() {
        let mut b = Blob::new(screen());
        let p0 = b.core.p;
        let sats0: Vec<Vec2> = b.sats.iter().map(|s| s.p).collect();
        for _ in 0..2400 {
            b.step(SUBSTEP);
        }
        assert!(b.all_finite());
        assert!((b.core.p - p0).len() < 1e-3, "core drifted: {:?}", b.core.p);
        for (i, s) in b.sats.iter().enumerate() {
            assert!(
                (s.p - sats0[i]).len() < 1e-3,
                "satellite {i} drifted to {:?} from {:?}",
                s.p,
                sats0[i]
            );
        }
        assert!(b.is_at_rest());
    }

    #[test]
    fn disturbed_springs_ring_down_to_rest() {
        let mut b = Blob::new(screen());
        for s in b.sats.iter_mut() {
            s.v = v2(400.0, -250.0);
        }
        for _ in 0..2400 {
            b.step(SUBSTEP);
        }
        assert!(b.all_finite());
        let max_rel = b
            .sats
            .iter()
            .map(|s| (s.v - b.core.v).len())
            .fold(0.0f32, f32::max);
        assert!(max_rel < REST_SPEED, "satellites still moving at {max_rel}");
    }

    #[test]
    fn throw_estimator_matches_a_known_track() {
        // Pointer moving at exactly (1200, -600) px/s, sampled at 250 Hz.
        let mut t = PointerTrack::new();
        let vel = v2(1200.0, -600.0);
        for i in 0..100 {
            let s = i as f64 / 250.0;
            t.push(s, v2(100.0, 800.0) + vel * s as f32);
        }
        let est = t.velocity(99.0 / 250.0);
        assert!(
            (est - vel).len() < 1.0,
            "estimated {:?}, expected {:?}",
            est,
            vel
        );
    }

    #[test]
    fn throw_estimator_ignores_samples_older_than_the_window() {
        // Sits still for 300 ms, then flicks right for the last 80 ms. The estimate
        // must see the flick, not the average over the whole gesture.
        let mut t = PointerTrack::new();
        for i in 0..30 {
            t.push(i as f64 / 100.0, v2(500.0, 500.0));
        }
        for i in 1..=8 {
            let s = 0.30 + i as f64 / 100.0;
            t.push(s, v2(500.0 + 20.0 * i as f32, 500.0));
        }
        let est = t.velocity(0.38);
        assert!(est.x > 1500.0, "flick under-estimated: {:?}", est);
        assert!(est.y.abs() < 1.0, "spurious y velocity: {:?}", est);
    }

    #[test]
    fn throw_estimator_is_empty_without_samples() {
        let t = PointerTrack::new();
        assert_eq!(t.velocity(1.0), v2(0.0, 0.0));
    }

    #[test]
    fn edge_bounce_reverses_only_the_struck_component() {
        let b0 = screen();
        let mut b = Blob::new(b0);
        // Aim at the right wall with a downward component that must survive.
        b.core.p = v2(b0.x1 - COLLIDE_RADIUS - 1.0, 500.0);
        b.core.v = v2(1000.0, 300.0);
        b.snap_satellites();
        b.step(SUBSTEP);
        assert!(b.core.v.x < 0.0, "x should have reversed: {:?}", b.core.v);
        assert!(b.core.v.y > 0.0, "y should be untouched: {:?}", b.core.v);
        // Drag and friction are applied before the wall test in the same substep,
        // so the outgoing speed is a hair under `1000 * RESTITUTION`.
        let expect = 1000.0 * RESTITUTION;
        assert!(
            (b.core.v.x.abs() - expect).abs() < expect * 0.01,
            "restitution not applied: {:?}",
            b.core.v
        );
        assert!(b.core.p.x <= b0.x1 - COLLIDE_RADIUS + 1e-3);

        // ...and the same on the top wall.
        let mut b = Blob::new(b0);
        b.core.p = v2(700.0, b0.y0 + COLLIDE_RADIUS + 1.0);
        b.core.v = v2(-400.0, -900.0);
        b.snap_satellites();
        b.step(SUBSTEP);
        assert!(b.core.v.y > 0.0, "y should have reversed: {:?}", b.core.v);
        assert!(b.core.v.x < 0.0, "x should be untouched: {:?}", b.core.v);
    }

    #[test]
    fn stretch_is_capped_and_area_preserving() {
        let mut b = Blob::new(screen());
        b.core.v = v2(100000.0, 0.0);
        let s = b.stretch();
        assert!((s - MAX_STRETCH).abs() < 1e-5, "stretch not capped: {s}");
        // The satellite at local angle 0 lies along +x, so it stretches by exactly s;
        // the one at 90 degrees lies along +y and must shrink by 1/s.
        let along = b.rest_world(0) - b.core.p;
        let across = b.rest_world(SAT_COUNT / 4) - b.core.p;
        assert!((along.len() - SAT_ORBIT * s).abs() < 0.01);
        assert!((across.len() - SAT_ORBIT / s).abs() < 0.01);
    }

    #[test]
    fn grab_pulls_the_core_without_teleporting_it() {
        let mut b = Blob::new(screen());
        let start = b.core.p;
        b.grab(start);
        b.drag_to(start + v2(400.0, 0.0));
        b.step(SUBSTEP);
        let moved = (b.core.p - start).len();
        assert!(moved > 0.0, "core did not move");
        assert!(moved < 40.0, "core teleported {moved} px in one substep");
        for _ in 0..600 {
            b.step(SUBSTEP);
        }
        assert!((b.core.p - (start + v2(400.0, 0.0))).len() < 1.0);
    }

    #[test]
    fn hit_rects_cover_the_blob_and_stay_small() {
        let b = Blob::new(screen());
        let rects = b.hit_rects();
        assert!(!rects.is_empty());
        assert!(rects.len() < 64, "too many rects: {}", rects.len());
        assert!(b.hit_test(b.core.p), "centre is not grabbable");
        // A point well clear of the blob must be click-through.
        assert!(!b.hit_test(b.core.p + v2(400.0, 0.0)));
        // The grab margin really is outside the rendered isosurface.
        let just_outside = b.core.p + v2(b.iso_radius(1.0) + 8.0, 0.0);
        assert!(b.field(just_outside) < 1.0, "test point is inside the metal");
        assert!(b.hit_test(just_outside), "no grab margin outside the metal");
    }

    #[test]
    fn a_fully_stretched_blob_does_not_split_into_lumps() {
        // The visual signature of the whole toy is the blob elongating as it flies.
        // If the metaball union breaks apart at full stretch it renders as separate
        // beads instead of one pool of metal, which no amount of shading can fix.
        let mut b = Blob::new(screen());
        b.core.v = v2(100_000.0, 0.0); // far past the stretch cap
        b.snap_satellites();
        assert!((b.stretch() - MAX_STRETCH).abs() < 1e-5);

        // Walk the major axis from one end of the blob to the other; the field must
        // never dip below the isosurface in between, or the body has separated.
        let half = SAT_ORBIT * MAX_STRETCH + SAT_RADIUS;
        let mut min_f = f32::INFINITY;
        for i in -200..=200 {
            let x = half * (i as f32 / 200.0);
            min_f = min_f.min(b.field(b.core.p + v2(x, 0.0)));
        }
        assert!(min_f > 1.0, "blob split along its major axis: min field {min_f:.3}");

        // And across the minor axis, which the area-preserving squash makes thinner.
        let halfy = SAT_ORBIT / MAX_STRETCH + SAT_RADIUS;
        let mut min_fy = f32::INFINITY;
        for i in -200..=200 {
            let y = halfy * (i as f32 / 200.0);
            min_fy = min_fy.min(b.field(b.core.p + v2(0.0, y)));
        }
        assert!(min_fy > 1.0, "blob split along its minor axis: min field {min_fy:.3}");
    }

    #[test]
    fn collide_radius_matches_the_rendered_surface() {
        // If you retune CORE_RADIUS / SAT_RADIUS / SAT_ORBIT, this is the test that
        // tells you COLLIDE_RADIUS needs to move with them. The metaball union is
        // much larger than any single ball, so it cannot be eyeballed.
        let b = Blob::new(screen());
        let r = b.iso_radius(1.0);
        assert!(
            (r - COLLIDE_RADIUS).abs() < COLLIDE_RADIUS * 0.06,
            "rendered isosurface is at {r:.1} px but COLLIDE_RADIUS is {COLLIDE_RADIUS}"
        );
        // And the grab threshold must sit outside the metal, never inside it.
        assert!(b.iso_radius(HIT_ISO) > r + 6.0);
    }

    #[test]
    fn field_isosurface_is_where_we_think_it_is() {
        let b = Blob::new(screen());
        assert!(b.field(b.core.p) > 1.0);
        assert!(b.field(b.core.p + v2(1000.0, 0.0)) < 1.0);
    }

    // -----------------------------------------------------------------------
    // Doors instead of walls
    // -----------------------------------------------------------------------

    /// A blob thrown at the right-hand edge with no peer there. The old behaviour,
    /// and the one every failure mode has to fall back to.
    #[test]
    fn an_edge_with_nobody_behind_it_is_still_a_wall() {
        let mut b = Blob::new(screen());
        b.core.v = v2(3000.0, 0.0);
        assert_eq!(b.portals(), 0);
        // Containment at every step, not just at the end: by the time it settles it
        // has bounced off both sides, so the final velocity says nothing.
        for _ in 0..2400 {
            b.step(SUBSTEP);
            assert_eq!(b.departing_edge(), None, "nowhere to go, so it cannot be leaving");
            assert!(
                b.core.p.x <= screen().x1 - COLLIDE_RADIUS + 1e-3
                    && b.core.p.x >= screen().x0 + COLLIDE_RADIUS - 1e-3,
                "the blob went through a wall to {:?}",
                b.core.p
            );
        }
        assert!(b.is_at_rest(), "it should have settled");
    }

    /// The same throw, with a peer on the right. The blob sails through the line it
    /// would have bounced off.
    #[test]
    fn an_edge_with_a_peer_behind_it_is_a_door() {
        let mut b = Blob::new(screen());
        b.set_portals(Edge::Right.bit());
        b.core.v = v2(3000.0, 0.0);

        let mut left_by = None;
        for _ in 0..600 {
            b.step(SUBSTEP);
            if left_by.is_none() {
                left_by = b.departing_edge();
            }
        }
        assert_eq!(left_by, Some(Edge::Right), "it never went through the door");
        assert!(b.all_finite());
        assert!(
            b.core.p.x > screen().x1 - COLLIDE_RADIUS,
            "it is still inside at {:?}",
            b.core.p
        );
        // Given long enough it is genuinely gone, so the frame loop can stop drawing it.
        for _ in 0..600 {
            b.step(SUBSTEP);
        }
        assert!(b.is_off_screen(), "bbox {:?} still overlaps the screen", b.bbox());
    }

    /// Only the edge that was opened is a door. The other three still bounce.
    #[test]
    fn a_door_on_one_edge_does_not_open_the_others() {
        let mut b = Blob::new(screen());
        b.set_portals(Edge::Right.bit());
        b.core.v = v2(0.0, -3000.0);
        for _ in 0..600 {
            b.step(SUBSTEP);
        }
        assert_eq!(b.departing_edge(), None);
        assert!(b.core.p.y >= screen().y0 + COLLIDE_RADIUS - 1e-3, "went out the top");
    }

    /// A blob being dragged is not leaving, however far past the edge the pointer
    /// goes — otherwise it would shoot off to the neighbour mid-drag.
    #[test]
    fn a_held_blob_never_goes_through_a_door() {
        let mut b = Blob::new(screen());
        b.set_portals(0xf);
        b.grab(b.core.p);
        b.drag_to(v2(5000.0, 540.0));
        for _ in 0..600 {
            b.step(SUBSTEP);
            assert_eq!(b.departing_edge(), None, "it left while it was being held");
        }
    }

    #[test]
    fn along_edge_reports_where_it_crossed() {
        let mut b = Blob::new(screen());
        b.core.p = v2(1000.0, 270.0); // a quarter of the way down a 1080 screen
        assert!((b.along_edge(Edge::Right) - 0.25).abs() < 1e-4);
        b.core.p = v2(480.0, 500.0); // a quarter of the way across a 1920 screen
        assert!((b.along_edge(Edge::Top) - 0.25).abs() < 1e-4);
    }

    /// An arriving blob starts outside and has to get itself on screen.
    #[test]
    fn an_arriving_blob_comes_onto_the_screen() {
        let mut b = Blob::arriving(screen(), Edge::Left, 0.25, v2(900.0, 0.0), &[]);
        assert!(b.is_entering());
        assert!(b.core.p.x < screen().x0, "it should start outside, not at {:?}", b.core.p);
        assert!((b.core.p.y - 270.0).abs() < 1.0, "it came in at the wrong height");

        for _ in 0..600 {
            b.step(SUBSTEP);
        }
        assert!(b.all_finite());
        assert!(!b.is_entering(), "it never finished arriving");
        assert!(b.core.p.x > screen().x0 + COLLIDE_RADIUS, "still outside at {:?}", b.core.p);
    }

    /// While it is on its way in it must not be mistaken for something on its way
    /// out — otherwise a caught blob is instantly thrown back and the two machines
    /// play the blob like a hot potato forever.
    #[test]
    fn an_arriving_blob_is_not_immediately_thrown_back() {
        let mut b = Blob::arriving(screen(), Edge::Left, 0.5, v2(900.0, 0.0), &[]);
        b.set_portals(0xf);
        for _ in 0..600 {
            assert_eq!(b.departing_edge(), None, "it turned round and left again");
            b.step(SUBSTEP);
            if !b.is_entering() {
                return;
            }
        }
        panic!("it never got inside");
    }

    /// A peer that sends a velocity pointing the wrong way — buggy, or just a very
    /// soft throw between mismatched screens — must not strand the blob off the
    /// edge where nobody can reach it.
    #[test]
    fn a_throw_that_arrives_pointing_the_wrong_way_still_lands() {
        for (edge, bad) in [
            (Edge::Left, v2(-900.0, 0.0)),
            (Edge::Right, v2(900.0, 0.0)),
            (Edge::Top, v2(0.0, -900.0)),
            (Edge::Bottom, v2(0.0, 900.0)),
            (Edge::Left, v2(0.0, 0.0)),
        ] {
            let mut b = Blob::arriving(screen(), edge, 0.5, bad, &[]);
            for _ in 0..1200 {
                b.step(SUBSTEP);
            }
            assert!(!b.is_entering(), "{edge:?} with {bad:?} never came on screen");
            assert!(b.all_finite());
        }
    }

    /// The wobble has to survive the trip. This is the whole point of putting the
    /// satellites on the wire: the blob turns up still deformed by the throw.
    #[test]
    fn the_thrown_shape_survives_the_round_trip() {
        let mut sent = Blob::new(screen());
        sent.core.v = v2(1800.0, -600.0);
        sent.grab(sent.core.p);
        sent.drag_to(v2(1400.0, 300.0));
        for _ in 0..120 {
            sent.step(SUBSTEP);
        }
        sent.release(v2(1800.0, -600.0));
        for _ in 0..30 {
            sent.step(SUBSTEP);
        }

        let states = sent.sat_states();
        assert_eq!(states.len(), SAT_COUNT);
        let deformed = states.iter().any(|s| {
            (v2(s.off_x, s.off_y).len() - 1.0).abs() > 0.05 || v2(s.vel_x, s.vel_y).len() > 0.05
        });
        assert!(deformed, "the test did not actually deform the blob, so it proves nothing");

        let got = Blob::arriving(screen(), Edge::Left, 0.5, v2(1800.0, -600.0), &states);
        for (i, s) in states.iter().enumerate() {
            let off = (got.sats[i].p - got.core.p) * (1.0 / SAT_ORBIT);
            let rel = (got.sats[i].v - got.core.v) * (1.0 / SAT_ORBIT);
            assert!(
                (off - v2(s.off_x, s.off_y)).len() < 1e-4,
                "satellite {i} arrived at the wrong offset"
            );
            assert!((rel - v2(s.vel_x, s.vel_y)).len() < 1e-4, "satellite {i} lost its motion");
        }
    }

    /// A peer that models its blob with a different number of satellites, or none
    /// at all, still gets a landed throw — just a round one.
    #[test]
    fn a_throw_from_a_differently_shaped_peer_still_lands() {
        for n in [0usize, 3, SAT_COUNT + 5] {
            let junk = vec![crate::wire::SatState { off_x: 9.0, off_y: 9.0, ..Default::default() }; n];
            let mut b = Blob::arriving(screen(), Edge::Top, 0.5, v2(0.0, 900.0), &junk);
            for _ in 0..1200 {
                b.step(SUBSTEP);
            }
            assert!(b.all_finite(), "{n} satellites produced a broken blob");
            assert!(b.max_sat_dist() <= MAX_SAT_DIST + 1e-3);
            assert!(!b.is_entering());
        }
    }

    /// The throw did not connect. The blob has to come back on screen under its own
    /// power rather than being teleported to the wall.
    #[test]
    fn a_throw_that_is_refused_bounces_the_blob_back_in() {
        let mut b = Blob::new(screen());
        b.set_portals(Edge::Right.bit());
        b.core.v = v2(3000.0, 0.0);
        let mut edge = None;
        while edge.is_none() {
            b.step(SUBSTEP);
            edge = b.departing_edge();
        }
        // Let it get properly outside, the way a real one would while waiting.
        for _ in 0..120 {
            b.step(SUBSTEP);
        }
        let outside = b.core.p;
        assert!(outside.x > screen().x1);

        b.bounce_back(Edge::Right);
        assert!(b.is_entering());
        assert_eq!(b.core.p, outside, "bouncing back must not teleport it");
        assert!(b.core.v.x < 0.0, "it should be heading back in");

        for _ in 0..2400 {
            b.step(SUBSTEP);
        }
        assert!(b.all_finite());
        assert!(!b.is_entering(), "it never made it back");
        assert!(b.core.p.x < screen().x1 - COLLIDE_RADIUS + 1e-3);
    }

    /// Closing the door under a blob that is already outside must not lose it: the
    /// walls catch it and put it back.
    #[test]
    fn a_peer_vanishing_mid_throw_does_not_lose_the_blob() {
        let mut b = Blob::new(screen());
        b.set_portals(Edge::Right.bit());
        b.core.v = v2(3000.0, 0.0);
        for _ in 0..400 {
            b.step(SUBSTEP);
        }
        assert!(b.core.p.x > screen().x1 - COLLIDE_RADIUS);

        b.set_portals(0); // the peer went away
        for _ in 0..2400 {
            b.step(SUBSTEP);
        }
        assert!(b.all_finite());
        let bb = b.bbox();
        assert!(
            bb.x1 <= screen().x1 + 1.0 && bb.x0 >= screen().x0 - 1.0,
            "the blob was left outside at {bb:?}"
        );
    }
}
