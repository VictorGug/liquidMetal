//! `--selftest`: drive the physics headlessly through a scripted grab / fling /
//! bounce sequence and check the invariants that matter.
//!
//! No SDL, no GL, no X. This is the part of the program that can be verified without
//! being able to see the screen, so it is worth having teeth.

use crate::physics::*;

/// Total substeps to run. 10 000 at 240 Hz is ~41.7 s of simulated time.
const TICKS: usize = 10_000;
/// Seconds after the last input by which the blob must have stopped.
const REST_DEADLINE: f32 = 8.0;

// The script, in ticks: sit still, grab, whip the pointer, release, coast, bounce.
const IDLE_UNTIL: usize = 240; // 1.0 s sitting still
const GRAB_AT: usize = 240;
const RELEASE_AT: usize = 400; // ~0.67 s of dragging

/// Which scripted phase a tick falls in — turns a bare number in a FAIL line into
/// something you can act on.
fn phase_of(tick: usize) -> &'static str {
    if tick < GRAB_AT {
        "idle"
    } else if tick < RELEASE_AT {
        "drag"
    } else {
        "free flight"
    }
}

struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

pub fn run() -> bool {
    let bounds = Bounds::screen(1920.0, 1080.0);
    let mut blob = Blob::new(bounds);
    let mut track = PointerTrack::new();

    // --- observations gathered while stepping ---
    let mut any_non_finite = false;
    let mut worst_out_of_bounds = 0.0f32;
    let mut max_sat_dist = 0.0f32;
    let mut max_sat_tick = 0usize;
    let mut bounces = 0usize;
    let mut prev_v = blob.core.v;
    let mut idle_start_pos = blob.core.p;
    let mut drag_end_pos = blob.core.p;
    let mut release_speed = 0.0f32;
    let mut rest_after_release: Option<f32> = None;
    let mut released_at_tick: usize = 0;

    #[allow(unused_assignments)]
    let mut cursor;

    for tick in 0..TICKS {
        let t = tick as f32 * SUBSTEP;
        let t64 = t as f64;

        // ---- scripted input ----
        if tick == GRAB_AT {
            cursor = blob.core.p;
            track.clear();
            track.push(t64, cursor);
            blob.grab(cursor);
        }
        if (GRAB_AT..RELEASE_AT).contains(&tick) {
            // Whip the pointer up and to the right, accelerating, so the release
            // sees a genuinely fast track rather than a constant crawl.
            let u = (tick - GRAB_AT) as f32 * SUBSTEP;
            cursor = idle_start_pos + v2(1500.0 * u * u, -900.0 * u * u);
            track.push(t64, cursor);
            blob.drag_to(cursor);
        }
        if tick == RELEASE_AT {
            drag_end_pos = blob.core.p;
            let throw = track.velocity(t64);
            release_speed = throw.len();
            blob.release(throw);
            released_at_tick = tick;
        }

        blob.step(SUBSTEP);

        // ---- invariants ----
        if !blob.all_finite() {
            any_non_finite = true;
        }
        let p = blob.core.p;
        let over = [
            bounds.x0 + COLLIDE_RADIUS - p.x,
            p.x - (bounds.x1 - COLLIDE_RADIUS),
            bounds.y0 + COLLIDE_RADIUS - p.y,
            p.y - (bounds.y1 - COLLIDE_RADIUS),
        ];
        for o in over {
            if o.is_finite() && o > worst_out_of_bounds {
                worst_out_of_bounds = o;
            }
        }
        let msd = blob.max_sat_dist();
        if msd > max_sat_dist {
            max_sat_dist = msd;
            max_sat_tick = tick;
        }

        // A bounce is a sign flip on either axis while moving freely.
        if tick > RELEASE_AT
            && ((prev_v.x > 1.0 && blob.core.v.x < -1.0)
                || (prev_v.x < -1.0 && blob.core.v.x > 1.0)
                || (prev_v.y > 1.0 && blob.core.v.y < -1.0)
                || (prev_v.y < -1.0 && blob.core.v.y > 1.0))
        {
            bounces += 1;
        }
        prev_v = blob.core.v;

        if tick < IDLE_UNTIL {
            idle_start_pos = blob.core.p;
        }
        if tick > RELEASE_AT && rest_after_release.is_none() && blob.is_at_rest() {
            rest_after_release = Some((tick - released_at_tick) as f32 * SUBSTEP);
        }
    }

    let drag_distance = (drag_end_pos - idle_start_pos).len();

    let checks = vec![
        Check {
            name: "no NaN or infinity anywhere over 10000 ticks",
            ok: !any_non_finite && blob.all_finite(),
            detail: format!(
                "core={:?} v={:?}",
                (blob.core.p.x, blob.core.p.y),
                (blob.core.v.x, blob.core.v.y)
            ),
        },
        Check {
            name: "blob core never leaves the screen bounds",
            ok: worst_out_of_bounds < 0.5,
            detail: format!("worst overshoot {worst_out_of_bounds:.3} px (tolerance 0.5)"),
        },
        Check {
            name: "satellites stay a sane distance from the core",
            ok: max_sat_dist < MAX_SAT_DIST * 0.95,
            detail: format!(
                "max {max_sat_dist:.1} px at t={:.2} s ({}), rest orbit {SAT_ORBIT:.0} px, hard cap {MAX_SAT_DIST:.0} px",
                max_sat_tick as f32 * SUBSTEP,
                phase_of(max_sat_tick)
            ),
        },
        Check {
            name: "drag actually moved the blob",
            ok: drag_distance > 200.0,
            detail: format!("core travelled {drag_distance:.0} px while held"),
        },
        Check {
            name: "release produced a real throw velocity",
            ok: release_speed > 500.0 && release_speed <= MAX_THROW_SPEED,
            detail: format!("{release_speed:.0} px/s from the {:.0} ms window", THROW_WINDOW * 1000.0),
        },
        Check {
            name: "blob bounced off at least one wall",
            ok: bounces > 0,
            detail: format!("{bounces} bounce(s)"),
        },
        Check {
            name: "comes to rest within a few seconds of the last input",
            ok: rest_after_release.map(|s| s <= REST_DEADLINE).unwrap_or(false),
            detail: match rest_after_release {
                Some(s) => format!("settled {s:.2} s after release (deadline {REST_DEADLINE:.1} s)"),
                None => "never settled".to_string(),
            },
        },
        Check {
            name: "still at rest at the end of the run",
            ok: blob.is_at_rest(),
            detail: format!("still for {:.2} s", blob.still_for),
        },
    ];

    println!("--- liquidMetal self-test: {TICKS} ticks at {} Hz ---", 1.0 / SUBSTEP);
    let mut all_ok = true;
    for c in &checks {
        println!(
            "  [{}] {:<52} {}",
            if c.ok { "PASS" } else { "FAIL" },
            c.name,
            c.detail
        );
        all_ok &= c.ok;
    }
    println!(
        "--- {} ---",
        if all_ok { "ALL CHECKS PASSED" } else { "SELF-TEST FAILED" }
    );
    all_ok
}
