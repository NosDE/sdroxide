// The cloud deck, as a stack of slices through the troposphere.
//
// Weather is a depth of air, not a picture stuck on a sphere, and the argument
// here is the one `solar_aurora.wgsl` makes one atmosphere higher up: hand the
// shader concentric spheres at real altitudes and let each contribute its own
// slice. What is different is that cloud *occludes*. The aurora emits zero
// alpha and only ever adds light; a deck hides the coastline under it, so these
// slices composite, and the CPU hands them over bottom-up so the blend runs
// back to front.
//
// Nothing about the vertical structure is invented. `cloud_tex` carries, per
// column, how thick the cloud is and how *high its top stands* — the second
// straight out of the infrared mosaic, because a cloud top's temperature is its
// altitude. So a thunderhead towers over the stratus beside it for the same
// reason it does in the sky, and a shell only contributes where it is inside
// the cloud that column actually has.
//
// The lightning is the one invention, and it is confined to the timing. Where
// the storms are, how large, how tall and how often each flashes all come from
// the same mosaic; which millisecond a given stroke fires does not, because no
// free worldwide feed of real strikes exists. The flashes light the cloud from
// inside rather than being drawn as marks on it, which is why an anvil goes
// bright from below.

struct Globals {
    view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,   // xyz eye, w near
    sun_pos: vec4<f32>,      // xyz centre, w rendered radius
    sun_to_earth: vec4<f32>,
    solar_north: vec4<f32>,
    viewport: vec4<f32>,
    misc: vec4<f32>,         // x seconds, y photo blend, zw spare
};

struct DrawData {
    model: mat4x4<f32>,
    basis: mat4x4<f32>,
    tint: vec4<f32>,
    tint2: vec4<f32>,
    // x this shell's altitude (km), y the slab of air it stands for (km),
    // z intensity (screen-size fade), w unused on this path.
    params: vec4<f32>,
    // x vertical exaggeration, y deck floor (km), z deck ceiling (km),
    // w the Earth's rendered radius, in world units.
    style: vec4<f32>,
};

/// Up to eight flashes alight at once — see `MAX_FLASHES` in `scene.rs`.
struct Flashes {
    // xyz world position inside the tower, w brightness. Zero is an unused
    // slot, which contributes nothing, so the loop below never branches.
    items: array<vec4<f32>, 8>,
    // x how far the light reaches, in world units.
    reach: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(7) var cloud_tex: texture_2d<f32>;
@group(0) @binding(8) var<uniform> fl: Flashes;
@group(1) @binding(0) var<uniform> d: DrawData;

/// Cloud-top height at the top of the stored range, kilometres. Must match
/// `clouds::TOP_MAX_KM`.
const TOP_MAX_KM = 18.0;

/// A sunlit cloud top reflects about seven tenths of what falls on it, against
/// the ocean's six hundredths. If the deck is not markedly brighter than the
/// sea it reads as smoke rather than as cloud.
const ALBEDO = vec3<f32>(0.93, 0.95, 0.99);
/// A flash is a spark gap, so its light is blue-white.
const FLASH_TINT = vec3<f32>(0.80, 0.87, 1.00);
/// What the night side keeps. Not zero: an unlit deck still has to occlude the
/// coastline glow under it, or the land shows straight through the weather.
const NIGHT_FLOOR = 0.035;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3(0.055)) / 1.055, vec3(2.4));
    return select(hi, lo, c <= vec3(0.04045));
}

fn hash3(p: vec3<f32>) -> f32 {
    return fract(sin(dot(p, vec3(12.9898, 78.233, 37.719))) * 43758.5453);
}

fn vnoise(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let c000 = hash3(i + vec3(0.0, 0.0, 0.0));
    let c100 = hash3(i + vec3(1.0, 0.0, 0.0));
    let c010 = hash3(i + vec3(0.0, 1.0, 0.0));
    let c110 = hash3(i + vec3(1.0, 1.0, 0.0));
    let c001 = hash3(i + vec3(0.0, 0.0, 1.0));
    let c101 = hash3(i + vec3(1.0, 0.0, 1.0));
    let c011 = hash3(i + vec3(0.0, 1.0, 1.0));
    let c111 = hash3(i + vec3(1.0, 1.0, 1.0));
    let x00 = mix(c000, c100, u.x);
    let x10 = mix(c010, c110, u.x);
    let x01 = mix(c001, c101, u.x);
    let x11 = mix(c011, c111, u.x);
    return mix(mix(x00, x10, u.y), mix(x01, x11, u.y), u.z);
}

/// Billows. Sampled from the direction *and* the altitude, so two shells agree
/// about the same lump of cloud and the stack reads as one body of air instead
/// of as a pile of spheres. The 1° mosaic cannot resolve an individual cell, so
/// this is the texture between its pixels — multiplicative and centred on one,
/// so it can shape what the picture says but never put cloud in a clear sky.
fn billows(n: vec3<f32>, alt: f32, t: f32) -> f32 {
    let drift = t * 0.004;
    let p = n * 190.0 + vec3(0.0, 0.0, alt * 0.55) + vec3(drift, drift * 0.3, 0.0);
    let coarse = vnoise(p * 0.35);
    let fine = vnoise(p * 1.4);
    return clamp(0.42 + 1.05 * coarse + 0.38 * fine, 0.0, 2.0);
}

/// How deep the cloud in this column is, kilometres.
///
/// A thin cirrus shield is a sheet a kilometre thick with its top at eleven; a
/// storm reaches from near the ground to near the tropopause. Optical thickness
/// is what separates them, and it is the other thing the mosaic measured.
fn deck_depth(opacity: f32, top_km: f32) -> f32 {
    return clamp(1.0 + opacity * top_km * 0.85, 0.8, top_km);
}

/// Light from the storms, at a point in the deck.
///
/// Fixed cost and uniform control flow: an unused slot has zero brightness and
/// adds nothing, so there is nothing to branch on.
fn flash_light(world: vec3<f32>, convective: f32) -> f32 {
    var acc = 0.0;
    for (var i = 0u; i < 8u; i++) {
        let f = fl.items[i];
        let dist = length(world - f.xyz);
        acc += f.w * exp(-dist / max(fl.reach.x, 1e-6));
    }
    // Only cloud that is deep enough to be making the lightning lights up with
    // it. A flash that lit the cirrus half a continent away would be a lamp in
    // the sky rather than a storm.
    return acc * convective;
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) nrm: vec3<f32>,
    @location(2) world: vec3<f32>,
    /// The same direction in the Earth's own frame. The billows are sampled from
    /// this rather than from the world-space normal, so the fine structure turns
    /// with the planet instead of crawling across its surface all day.
    @location(3) body: vec3<f32>,
};

@vertex
fn vs(@location(0) pos: vec3<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var o: VsOut;
    let world = d.model * vec4(pos, 1.0);
    o.clip = g.view_proj * world;
    o.world = world.xyz;
    o.nrm = normalize((d.basis * vec4(pos, 0.0)).xyz);
    o.body = normalize(pos);
    o.uv = uv;
    return o;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    // The grid is exactly the picture the mosaic was requested as, laid out on
    // the same equirectangular convention as the sphere's own UVs, so this is a
    // straight tap with no correction.
    let c = textureSample(cloud_tex, samp, in.uv);
    let opacity = c.r;
    let top_km = c.g * TOP_MAX_KM;

    // Most columns are clear or as good as, and leaving early there is what
    // makes a stack of eighteen full spheres affordable. It is also what draws
    // *nothing* poleward of about 73°, where no geostationary satellite looks:
    // blank is the honest answer there, and a clear sky would be a claim.
    if (opacity < 0.02) {
        discard;
    }

    let alt = d.params.x;
    let base_km = max(d.style.y, top_km - deck_depth(opacity, top_km));
    // This shell only contributes where it is inside the cloud this column
    // actually has. That single line is the whole trick: a two-dimensional
    // height field, read at a series of altitudes, is a volume.
    let inside = smoothstep(base_km - 0.5, base_km + 0.6, alt)
               * (1.0 - smoothstep(top_km - 0.7, top_km + 0.3, alt));
    if (inside < 0.004) {
        discard;
    }

    let n = normalize(in.nrm);
    let to_eye = normalize(g.camera_pos.xyz - in.world);
    let to_sun = normalize(g.sun_pos.xyz - in.world);

    // Density here, and from it how much of this slab is opaque. `slab` makes
    // it a Riemann sum, so the deck looks the same however many shells were
    // spent on it.
    // Path length through this slab: 1/cos of the incidence angle, exactly as
    // the aurora computes it. Looking along the deck instead of across it is
    // what turns the limb into a visible band of weather standing off the
    // surface — the whole reason for drawing this in the round.
    let grazing = min(1.0 / max(abs(dot(n, to_eye)), 0.20), 3.0);

    let dens = opacity * inside * billows(normalize(in.body), alt, g.misc.x);
    let a = clamp(1.0 - exp(-dens * d.params.y * grazing * 0.55), 0.0, 1.0) * d.params.z;
    if (a < 0.002) {
        discard;
    }

    // The same soft terminator the ground uses (`solar_body.wgsl`). If the
    // cloud's day/night line does not sit exactly on the planet's, the deck
    // reads as floating above it.
    let day = smoothstep(-0.06, 0.16, dot(n, to_sun));

    // Self-shadow: how much cloud stands between this sample and the top of its
    // own column. It is why a deck looks three-dimensional instead of like fog
    // — the underside of a tower is dark and its anvil is white.
    let above = max(top_km - alt, 0.0) / max(top_km - base_km, 0.1);
    let shade = mix(1.0, 0.30, clamp(above * opacity, 0.0, 1.0));

    // The silver lining: light that has come the long way through the edge of a
    // cloud, which is why the rim facing the Sun is the brightest thing in the
    // sky.
    let forward = pow(max(dot(-to_eye, -to_sun), 0.0), 8.0) * 0.45;

    var col = srgb_to_linear(ALBEDO) * (NIGHT_FLOOR + day * (0.95 * shade + forward));
    // Lightning, added as light rather than as more cloud: it brightens the
    // storm without making it thicker. `convective` is read off the height
    // field, so only the towers that are making the flashes light up with them.
    let convective = smoothstep(0.55, 0.80, c.g);
    col += srgb_to_linear(FLASH_TINT) * flash_light(in.world, convective) * 0.9;

    // Premultiplied: the colour is already scaled by the coverage it stands for.
    return vec4(col * a, a);
}
