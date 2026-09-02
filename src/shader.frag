#version 330 core

// Liquid-metal blob.
//
// One fullscreen triangle, all the work here. The pipeline is:
//   metaball field  ->  analytic gradient  ->  surface normal
//               ->  procedural studio environment reflection + fresnel.
//
// There is no diffuse term anywhere. This is a metal: everything you see is the
// environment reflected off the normal. Nothing is sampled from behind the window
// either — a transparent overlay cannot see the desktop under it, so there is no
// refraction, and pretending otherwise would just look wrong.

out vec4 fragColor;

uniform vec2  uResolution;   // drawable size in pixels
uniform float uTime;         // seconds since start
uniform int   uCount;        // active entries in uBlobs
uniform vec4  uBlobs[40];    // xy = centre in top-down pixels, z = radius, w = strength
uniform int   uOpaque;       // 1 in --windowed: composite over a checkerboard instead

// --------------------------------------------------------------------------
// TUNABLES — the whole look lives in this block.
// --------------------------------------------------------------------------

const float FIELD_EPS   = 1.0;    // softens the singularity at each ball's centre
// How much to soften the field *for shading only*, in units of each ball's r^2.
// The raw 1/d^2 field has a singularity at every ball centre, and the normal built
// from it carves a visible bead into the surface at each satellite — the blob reads
// as eight balls in a bag instead of one pool of metal. Far from the balls the
// softened and raw fields agree, so the silhouette and its antialiased rim are
// untouched; only the interior shading is smoothed. Raise it if beads reappear.
const float NORMAL_SOFT = 0.65;
const float EDGE_SOFT   = 1.0;    // multiplier on the fwidth-derived AA width
// >1 keeps the interior flatter and pushes the curvature out toward the rim, which
// reads as a pool of mercury rather than a ball bearing — and it stops the whole
// reflection converging on a single point in the middle.
const float NORMAL_POW  = 1.30;

const vec3  FLOOR_COLOR   = vec3(0.055, 0.065, 0.085);  // dark cool ground
const vec3  SKY_COLOR     = vec3(0.50,  0.58,  0.72);   // pale cool sky
const vec3  HORIZON_COLOR = vec3(1.00,  0.97,  0.92);   // the bright band
const float HORIZON_WIDTH = 0.19;
const float HORIZON_GAIN  = 0.80;
const vec3  CEILING_COLOR = vec3(0.75,  0.80,  0.92);   // soft strip above the horizon
const float CEILING_GAIN  = 0.30;
// Dim bounce light off the floor, so the lower hemisphere reads as a dark room
// rather than as a hole punched in the blob.
const vec3  FLOOR_BOUNCE  = vec3(0.10, 0.12, 0.16);
// Two soft vertical softbox strips. Elevation-only environments make a chrome ball
// look like a cheap gradient; azimuthal structure is most of what sells it as a
// reflection of an actual room.
const vec3  STRIP_COLOR   = vec3(0.90, 0.94, 1.00);
const float STRIP_A_DIR   = 0.90;   // azimuth, radians
const float STRIP_A_WIDTH = 0.35;
const float STRIP_A_GAIN  = 0.38;
const float STRIP_B_DIR   = -2.10;
const float STRIP_B_WIDTH = 0.28;
const float STRIP_B_GAIN  = 0.26;

// Two studio softboxes. Tighter exponent = smaller, sharper highlight.
const vec3  KEY_DIR    = vec3(-0.4735, 0.7237, 0.5021);
const vec3  KEY_COLOR  = vec3(1.00, 0.99, 0.96);
const float KEY_TIGHT  = 64.0;
const float KEY_GAIN   = 1.50;
const vec3  FILL_DIR   = vec3(0.6217, 0.3008, 0.7222);
const vec3  FILL_COLOR = vec3(0.86, 0.92, 1.00);
const float FILL_TIGHT = 220.0;
const float FILL_GAIN  = 2.40;

// Base reflectance of the metal, very slightly cool. Fresnel drives it to white.
const vec3  METAL_TINT = vec3(0.86, 0.89, 0.95);
const vec3  RIM_COLOR  = vec3(0.72, 0.80, 0.95);
const float RIM_GAIN   = 0.35;

const float RIPPLE_AMOUNT = 0.030;  // keep restrained: this is chrome, not lava
const float RIPPLE_SPEED  = 1.00;

const float EXPOSURE = 1.35;

// --------------------------------------------------------------------------

// Field and its analytic gradient in one pass.
//
// The gradient is summed per blob rather than taken with dFdx/dFdy, because the
// screen-space derivative degenerates exactly where it matters most — the rim,
// where the field is changing fastest and the normal turns hardest.
// Returns the true field (isosurface at 1.0) and, separately, the softened field
// and its analytic gradient used for shading.
float fieldAndGrad(vec2 p, out vec2 grad, out float shadeField) {
    float f = 0.0;
    shadeField = 0.0;
    grad = vec2(0.0);
    for (int i = 0; i < uCount; ++i) {
        vec2  c  = uBlobs[i].xy;
        float r  = uBlobs[i].z;
        float w  = uBlobs[i].w;
        vec2  d  = p - c;
        float dd = dot(d, d);
        float k  = w * r * r;

        f += k / (dd + FIELD_EPS);

        // Same field, softened core. Used for the normal only.
        float ddn = dd + NORMAL_SOFT * r * r;
        shadeField += k / ddn;
        grad       += (-2.0 * k / (ddn * ddn)) * d;   // d/dp of k/(|d|^2 + soft)
    }
    return f;
}

// Two slow, low-frequency waves. Enough to read as a liquid surface, not enough
// to stop reading as polished.
vec2 ripple(vec2 p, float t) {
    float a = sin(p.x * 0.0210 + t * 0.61) + sin(p.x * 0.0130 - p.y * 0.0170 + t * 0.43);
    float b = cos(p.y * 0.0190 - t * 0.52) + cos(p.x * 0.0150 + p.y * 0.0110 - t * 0.37);
    return vec2(a, b) * 0.5;
}

// Procedural studio environment, sampled by reflection direction. +y is up.
vec3 environment(vec3 r) {
    // Tilt the horizon well off the view axis. With no tilt the blob's centre sits
    // exactly on the horizon band and the entire environment funnels into one point
    // there; pushing the centre up into the sky turns that funnel into the bright
    // horizon *ring* that reads as polished metal.
    float t = clamp(r.y * 0.90 + r.z * 0.42, -1.0, 1.0);

    vec3 c = mix(FLOOR_COLOR, SKY_COLOR, smoothstep(-0.62, 0.72, t));

    float fb = (t + 0.55) / 0.30;
    c += FLOOR_BOUNCE * exp(-fb * fb);

    float hb = t / HORIZON_WIDTH;
    c += HORIZON_COLOR * exp(-hb * hb) * HORIZON_GAIN;

    float cb = (t - 0.62) / 0.26;
    c += CEILING_COLOR * exp(-cb * cb) * CEILING_GAIN;

    // Vertical strips, placed by azimuth around the viewer.
    float az = atan(r.x, r.z);
    float sa = (az - STRIP_A_DIR) / STRIP_A_WIDTH;
    float sb = (az - STRIP_B_DIR) / STRIP_B_WIDTH;
    float above = smoothstep(-0.35, 0.55, t);
    c += STRIP_COLOR * exp(-sa * sa) * above * STRIP_A_GAIN;
    c += STRIP_COLOR * exp(-sb * sb) * above * STRIP_B_GAIN;

    c += KEY_COLOR  * pow(max(dot(r, normalize(KEY_DIR)),  0.0), KEY_TIGHT)  * KEY_GAIN;
    c += FILL_COLOR * pow(max(dot(r, normalize(FILL_DIR)), 0.0), FILL_TIGHT) * FILL_GAIN;
    return c;
}

void main() {
    // Blob positions arrive in screen pixels with y growing downward; gl_FragCoord
    // grows upward. Flip once here so everything below shares one convention.
    vec2 px = vec2(gl_FragCoord.x, uResolution.y - gl_FragCoord.y);

    vec2 grad;
    float shadeField;
    float f = fieldAndGrad(px, grad, shadeField);

    // Antialias from the screen-space derivative of the field, so the edge stays a
    // ~2 px ramp at any resolution instead of a hardcoded smoothstep width.
    float w = max(fwidth(f), 1e-5) * EDGE_SOFT;
    float alpha = smoothstep(-w, w, f - 1.0);

    if (alpha <= 0.0) {
        if (uOpaque == 1) {
            vec2 q = floor(px / 32.0);
            float chk = mod(q.x + q.y, 2.0);
            fragColor = vec4(mix(vec3(0.10), vec3(0.16), chk), 1.0);
        } else {
            fragColor = vec4(0.0);
        }
        return;
    }

    // For a lone metaball, field = (r/d)^2, so 1/sqrt(f) is exactly d/r — the sine
    // of the angle between the surface normal and the view axis on a sphere. It
    // stays well-behaved for the union, and needs no magic bump constant.
    float sn = pow(clamp(inversesqrt(max(shadeField, 1e-4)), 0.0, 1.0), NORMAL_POW);
    float cs = sqrt(max(1.0 - sn * sn, 0.0));

    // Gradient points toward increasing field, i.e. inward; the outward normal is
    // the other way. Negate y so that from here on +y is up.
    vec2 gdir = grad / max(length(grad), 1e-6);
    vec3 n = vec3(vec2(-gdir.x, gdir.y) * sn, cs);
    n.xy += ripple(px, uTime * RIPPLE_SPEED) * RIPPLE_AMOUNT * sn;
    n = normalize(n);

    // Orthographic view: everything is looked at straight down +z.
    const vec3 V = vec3(0.0, 0.0, 1.0);
    vec3 R = reflect(-V, n);

    // Schlick, with a coloured F0 because metals tint their reflection.
    float fr = pow(1.0 - clamp(n.z, 0.0, 1.0), 5.0);
    vec3 refl = METAL_TINT + (vec3(1.0) - METAL_TINT) * fr;

    vec3 col = environment(R) * refl;
    col += RIM_COLOR * fr * RIM_GAIN;

    col = vec3(1.0) - exp(-col * EXPOSURE);

    if (uOpaque == 1) {
        vec2 q = floor(px / 32.0);
        float chk = mod(q.x + q.y, 2.0);
        vec3 bg = mix(vec3(0.10), vec3(0.16), chk);
        fragColor = vec4(mix(bg, col, alpha), 1.0);
    } else {
        // Premultiplied alpha: X compositors expect it, and skipping this is what
        // produces dark halos around the silhouette.
        fragColor = vec4(col * alpha, alpha);
    }
}
