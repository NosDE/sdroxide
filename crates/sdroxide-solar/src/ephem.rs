//! Positions of the Sun, Earth and Moon, Earth's rotation, and the orientation
//! of the Sun's rotation axis.
//!
//! Algorithms are Meeus, *Astronomical Algorithms* (2nd ed.) — the low-accuracy
//! solar theory (ch. 25), the Astronomical Almanac's short lunar series, mean
//! sidereal time (ch. 12), mean obliquity (ch. 22) and the solar-disk elements
//! (ch. 29). Accuracy is far finer than a view that exaggerates body radii by
//! 20× can express; see `accuracy` on each function.
//!
//! # Frame
//!
//! Heliocentric ecliptic **of date**, right-handed: +X toward the vernal
//! equinox, +Z toward ecliptic north. Distances are in gigametres (10⁶ km), so
//! 1 AU ≈ 149.6 and the Earth's radius ≈ 0.0064 — the whole scene fits in
//! `[1e-3, 1e3]`, which stays exact in the f32 the GPU wants.
//!
//! Coordinates "of date" rather than J2000 because the low-accuracy solar
//! theory produces them naturally; precession is 0.014°/yr, invisible here, and
//! skipping the reduction removes a class of sign errors.

use crate::vec3::{Basis, Vec3, vec3};

/// Astronomical unit, gigametres.
pub const AU: f64 = 149.597_870_7;
/// Solar radius, gigametres.
pub const SUN_R: f64 = 0.695_700;
/// Earth mean radius, gigametres.
pub const EARTH_R: f64 = 0.006_371;
/// Moon mean radius, gigametres.
pub const MOON_R: f64 = 0.001_737_4;

/// Inclination of the Sun's equator to the ecliptic (Carrington).
const SUN_INCL_DEG: f64 = 7.25;

/// Sign of Stonyhurst longitude along `SunFrame::y`.
///
/// Stonyhurst longitude is **West-positive**, and the sub-Earth meridian drifts
/// slower than the Sun rotates, so a feature's longitude grows with time. With
/// `y = z × x` and prograde rotation about `+z`, that growth is along `+y`.
/// Verified by `stonyhurst_longitude_drifts_west` below, which propagates a
/// feature a day forward and checks the increase equals the synodic rate.
const STONYHURST_WEST_SIGN: f64 = 1.0;

/// Julian Day from a Unix timestamp (both UTC; the ~70 s of TT−UTC is well
/// below every tolerance here).
pub fn julian_day(unix_s: f64) -> f64 {
    unix_s / 86_400.0 + 2_440_587.5
}

/// Julian centuries since J2000.0.
pub fn centuries(jd: f64) -> f64 {
    (jd - 2_451_545.0) / 36_525.0
}

pub fn wrap360(d: f64) -> f64 {
    let r = d % 360.0;
    if r < 0.0 { r + 360.0 } else { r }
}

/// Wrap to `(-180, 180]`.
pub fn wrap180(d: f64) -> f64 {
    let r = wrap360(d);
    if r > 180.0 { r - 360.0 } else { r }
}

/// Mean obliquity of the ecliptic, degrees (Meeus 22.2). Accuracy 0.01″ over
/// ±2000 years of J2000.
pub fn obliquity_deg(jd: f64) -> f64 {
    let t = centuries(jd);
    23.439_291_111_1 - 0.013_004_166_7 * t - 1.638_9e-7 * t * t + 5.036_1e-7 * t * t * t
}

/// Greenwich *mean* sidereal time, degrees (Meeus 12.4).
///
/// Mean rather than apparent: the equation of the equinoxes is at most 1.1″,
/// which is 34 m at the equator — far below the resolution of the land mask.
pub fn gmst_deg(jd: f64) -> f64 {
    let t = centuries(jd);
    wrap360(
        280.460_618_37 + 360.985_647_366_29 * (jd - 2_451_545.0) + 0.000_387_933 * t * t
            - t * t * t / 38_710_000.0,
    )
}

/// Apparent geocentric longitude of the Sun (degrees) and the Earth–Sun
/// distance (AU). Meeus ch. 25 "low accuracy": ±0.01° in longitude.
pub fn sun_geocentric(jd: f64) -> (f64, f64) {
    let t = centuries(jd);
    // Geometric mean longitude and mean anomaly.
    let l0 = 280.466_46 + 36_000.769_83 * t + 0.000_303_2 * t * t;
    let m = (357.529_11 + 35_999.050_29 * t - 0.000_153_7 * t * t).to_radians();
    let e = 0.016_708_634 - 0.000_042_037 * t - 0.000_000_126_7 * t * t;
    // Equation of the centre.
    let c = (1.914_602 - 0.004_817 * t - 0.000_014 * t * t) * m.sin()
        + (0.019_993 - 0.000_101 * t) * (2.0 * m).sin()
        + 0.000_289 * (3.0 * m).sin();
    let true_lon = l0 + c;
    let true_anom = (m.to_degrees() + c).to_radians();
    let r = 1.000_001_018 * (1.0 - e * e) / (1.0 + e * true_anom.cos());
    // Reduce to the apparent longitude: aberration plus the longitude term of
    // nutation, both folded into Meeus's two-term correction.
    let omega = (125.04 - 1934.136 * t).to_radians();
    let lambda = true_lon - 0.005_69 - 0.004_78 * omega.sin();
    (wrap360(lambda), r)
}

/// Heliocentric position of the Earth, gigametres.
pub fn earth_heliocentric(jd: f64) -> Vec3 {
    let (lambda, r) = sun_geocentric(jd);
    // The Earth sits opposite the Sun's geocentric direction. The Earth's
    // ecliptic latitude as seen from the Sun is under 1″, so it is dropped.
    Vec3::from_lon_lat_deg(lambda + 180.0, 0.0) * (r * AU)
}

/// Geocentric ecliptic longitude and latitude of the Moon (degrees) and its
/// distance (gigametres).
///
/// The Astronomical Almanac's short series: ±0.3° in longitude, ±0.2° in
/// latitude, ±0.003 AU in distance. At the exaggerated radii this view uses,
/// 0.3° is a fraction of the rendered Moon.
pub fn moon_geocentric(jd: f64) -> (f64, f64, f64) {
    let t = centuries(jd);
    let s = |d: f64| d.to_radians().sin();
    let c = |d: f64| d.to_radians().cos();

    // Note the two arguments that run backwards (`259.3 − …`, `217.6 − …`):
    // sine is odd, so writing them the other way round silently flips those
    // terms' signs and costs ~2° of longitude.
    let lambda = 218.32 + 481_267.881 * t + 6.29 * s(477_198.87 * t + 135.0)
        - 1.27 * s(259.3 - 413_335.35 * t)
        + 0.66 * s(890_534.22 * t + 235.7)
        + 0.21 * s(954_397.74 * t + 269.9)
        - 0.19 * s(35_999.05 * t + 357.5)
        - 0.11 * s(966_404.03 * t + 186.6);

    let beta = 5.13 * s(483_202.02 * t + 93.3) + 0.28 * s(960_400.89 * t + 228.2)
        - 0.28 * s(6_003.15 * t + 318.3)
        - 0.17 * s(217.6 - 407_332.20 * t);

    // Equatorial horizontal parallax, degrees → distance via the Earth radius.
    let pi_deg = 0.9508
        + 0.0518 * c(477_198.85 * t + 135.0)
        + 0.0095 * c(413_335.38 * t - 259.3)
        + 0.0078 * c(890_534.23 * t + 235.7)
        + 0.0028 * c(954_397.70 * t + 269.9);
    // 6378.14 km is the equatorial radius the formula is defined against.
    let dist_km = 6378.14 / pi_deg.to_radians().sin();

    (wrap360(lambda), beta, dist_km / 1.0e6)
}

/// Heliocentric position of the Moon, gigametres.
pub fn moon_heliocentric(jd: f64) -> Vec3 {
    earth_heliocentric(jd) + moon_geocentric_vec(jd)
}

/// Earth→Moon vector in ecliptic coordinates, gigametres.
pub fn moon_geocentric_vec(jd: f64) -> Vec3 {
    let (lambda, beta, dist) = moon_geocentric(jd);
    Vec3::from_lon_lat_deg(lambda, beta) * dist
}

/// Longitude of the ascending node of the Sun's equator on the ecliptic,
/// degrees (Carrington, Meeus ch. 29).
fn sun_node_deg(jd: f64) -> f64 {
    73.666_667 + 1.395_833_3 * (jd - 2_396_758.0) / 36_525.0
}

/// Unit vector along the Sun's north rotational pole, in ecliptic coordinates.
///
/// The pole sits at ecliptic longitude `K − 90°`, latitude `90° − i`.
pub fn solar_north_ecliptic(jd: f64) -> Vec3 {
    let k = sun_node_deg(jd);
    Vec3::from_lon_lat_deg(k - 90.0, 90.0 - SUN_INCL_DEG)
}

/// The solar-disk elements as seen from Earth (Meeus ch. 29), degrees:
/// `(P, B0, L0)` — position angle of the rotation axis, heliographic latitude
/// of the disk centre, and its Carrington longitude.
pub fn solar_p_b0_l0(jd: f64) -> (f64, f64, f64) {
    let theta = wrap360((jd - 2_398_220.0) * 360.0 / 25.38);
    let i = SUN_INCL_DEG.to_radians();
    let k = sun_node_deg(jd);
    let (lambda, _) = sun_geocentric(jd);
    // Meeus uses the *geometric* longitude corrected for aberration only.
    let lambda_p = lambda - 0.005_69;
    let eps = obliquity_deg(jd).to_radians();

    let x = (-lambda_p.to_radians().cos() * eps.tan()).atan();
    let y = (-(lambda - k).to_radians().cos() * i.tan()).atan();
    let p = (x + y).to_degrees();

    let b0 = ((lambda - k).to_radians().sin() * i.sin()).asin().to_degrees();

    // η is the angle, measured prograde about the solar axis, from the equator's
    // ascending node on the ecliptic to the disk centre. Doing it with vectors
    // rather than `arctan(tan(λ−K)·cos I)` avoids the classic half-turn error:
    // the disk centre is the *Sun→Earth* direction, which is opposite the Sun's
    // geocentric longitude, and the naive arctangent lands 180° away.
    let node = Vec3::from_lon_lat_deg(k, 0.0);
    let north = solar_north_ecliptic(jd);
    let prograde = north.cross(node);
    let to_earth = Vec3::from_lon_lat_deg(lambda + 180.0, 0.0);
    let eta = to_earth.dot(prograde).atan2(to_earth.dot(node)).to_degrees();
    let l0 = wrap360(eta - theta);

    (p, b0, l0)
}

/// An orthonormal Stonyhurst basis for the Sun at a given instant.
///
/// `z` is solar north, `x` points at the sub-Earth point on the solar equator
/// (heliographic longitude 0), and `y = z × x` is heliographic **West**.
#[derive(Debug, Clone, Copy)]
pub struct SunFrame {
    pub basis: Basis,
    /// Unit vector from the Sun toward the Earth.
    pub to_earth: Vec3,
}

pub fn sun_frame(jd: f64) -> SunFrame {
    let z = solar_north_ecliptic(jd);
    let to_earth = earth_heliocentric(jd).normalize();
    // Project the Sun→Earth direction onto the solar equatorial plane: that is
    // the central meridian as seen from Earth.
    let x = (to_earth - z * z.dot(to_earth)).normalize();
    let y = z.cross(x);
    SunFrame { basis: Basis { x, y, z }, to_earth }
}

impl SunFrame {
    /// Unit direction of a heliographic (Stonyhurst) coordinate, in ecliptic
    /// coordinates. `lon_west_deg` is West-positive — the DONKI convention.
    pub fn direction(&self, lat_deg: f64, lon_west_deg: f64) -> Vec3 {
        let b = lat_deg.to_radians();
        let l = lon_west_deg.to_radians() * STONYHURST_WEST_SIGN;
        self.basis.x * (b.cos() * l.cos())
            + self.basis.y * (b.cos() * l.sin())
            + self.basis.z * b.sin()
    }

    /// Inverse of [`Self::direction`]: `(lat, lon_west)` in degrees.
    pub fn stonyhurst_of(&self, dir: Vec3) -> (f64, f64) {
        let v = self.basis.unapply(dir.normalize());
        let lat = v.z.clamp(-1.0, 1.0).asin().to_degrees();
        let lon = v.y.atan2(v.x).to_degrees() * STONYHURST_WEST_SIGN;
        (lat, lon)
    }

    /// Heliographic latitude of the disk centre, from the frame itself.
    pub fn b0_deg(&self) -> f64 {
        self.to_earth.dot(self.basis.z).clamp(-1.0, 1.0).asin().to_degrees()
    }
}

/// Sidereal solar rotation rate at a heliographic latitude, degrees/day
/// (Snodgrass & Ulrich 1990, magnetic-feature tracers).
pub fn sidereal_rotation_deg_per_day(lat_deg: f64) -> f64 {
    let s2 = lat_deg.to_radians().sin().powi(2);
    14.713 - 2.396 * s2 - 1.787 * s2 * s2
}

/// Mean rate at which the Earth's heliocentric longitude advances, degrees/day.
/// Subtract this from the sidereal rate to get the synodic (as-seen-from-Earth)
/// rate that Stonyhurst longitudes actually drift at.
pub const EARTH_MEAN_MOTION_DEG_PER_DAY: f64 = 0.985_647;

/// Orientation of the Earth: an orthonormal basis whose columns are the ECEF
/// axes expressed in ecliptic coordinates.
///
/// With this basis the mesh's body frame *is* ECEF, so the land-mask UVs, the
/// QTH marker and the terminator all share one coordinate system.
/// Rotate an equatorial-of-date vector into ecliptic-of-date coordinates.
///
/// A rotation by −ε about the X axis. Shared with the satellite tracker, whose
/// SGP4 output is in the equatorial TEME frame.
pub fn equatorial_to_ecliptic(v: Vec3, jd: f64) -> Vec3 {
    let eps = obliquity_deg(jd).to_radians();
    vec3(v.x, v.y * eps.cos() + v.z * eps.sin(), -v.y * eps.sin() + v.z * eps.cos())
}

pub fn earth_basis(jd: f64) -> Basis {
    let theta = gmst_deg(jd).to_radians();
    // ECEF → equatorial of date is a rotation by GMST about the polar axis;
    // equatorial → ecliptic is the obliquity rotation.
    let eq_to_ecl = |v: Vec3| equatorial_to_ecliptic(v, jd);
    Basis {
        x: eq_to_ecl(vec3(theta.cos(), theta.sin(), 0.0)),
        y: eq_to_ecl(vec3(-theta.sin(), theta.cos(), 0.0)),
        z: eq_to_ecl(vec3(0.0, 0.0, 1.0)),
    }
}

/// Orientation of the Moon: the mean Earth/polar-axis frame, as columns in
/// ecliptic coordinates.
///
/// The Moon is tidally locked, so its body frame is defined by where it is:
/// `x` points at the Earth (selenographic 0°, the centre of the near side),
/// `z` is the rotation axis, and `y` completes the pair, which puts
/// selenographic **east** — the Mare Crisium limb — along the direction of
/// orbital motion, exactly as it appears from the ground.
///
/// Cassini's laws put the axis within 1.54° of the ecliptic pole, so that is
/// what is used for `z`. Physical and optical libration (±8° in longitude,
/// ±7° in latitude) is not modelled: it rocks the near side by a few degrees
/// about a face that is otherwise always the same one.
pub fn moon_basis(jd: f64) -> Basis {
    let to_earth = (-moon_geocentric_vec(jd)).normalize();
    let pole = vec3(0.0, 0.0, 1.0);
    // Orthogonalise, so the frame stays exactly orthonormal as the Moon moves
    // through its 5° of ecliptic latitude.
    let z = (pole - to_earth * pole.dot(to_earth)).normalize();
    Basis { x: to_earth, y: z.cross(to_earth), z }
}

/// Unit vector of a geodetic latitude/longitude in the Earth's body (ECEF)
/// frame. Spherical: WGS-84 flattening is 0.3%, invisible at any exaggeration
/// this view offers.
pub fn geodetic_to_body(lat_deg: f64, lon_deg: f64) -> Vec3 {
    let (la, lo) = (lat_deg.to_radians(), lon_deg.to_radians());
    vec3(la.cos() * lo.cos(), la.cos() * lo.sin(), la.sin())
}

/// Geographic latitude/longitude of the point where the Sun is overhead.
pub fn subsolar_point(jd: f64) -> (f64, f64) {
    let basis = earth_basis(jd);
    // Earth → Sun in ecliptic coordinates, expressed in the ECEF frame.
    let to_sun = basis.unapply((-earth_heliocentric(jd)).normalize());
    (to_sun.z.clamp(-1.0, 1.0).asin().to_degrees(), to_sun.y.atan2(to_sun.x).to_degrees())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JD → the Unix seconds our public API takes.
    fn unix_of_jd(jd: f64) -> f64 {
        (jd - 2_440_587.5) * 86_400.0
    }

    #[test]
    fn julian_day_round_trips() {
        // 2000-01-01T12:00:00Z is J2000.0 exactly.
        assert!((julian_day(946_728_000.0) - 2_451_545.0).abs() < 1e-9);
        assert!((julian_day(0.0) - 2_440_587.5).abs() < 1e-9);
        let jd = 2_446_895.5;
        assert!((julian_day(unix_of_jd(jd)) - jd).abs() < 1e-9);
    }

    #[test]
    fn obliquity_matches_meeus_22a() {
        // 1987 April 10.0 TD → ε0 = 23°26′27.407″.
        let e = obliquity_deg(2_446_895.5);
        assert!((e - 23.440_946).abs() < 1e-5, "ε = {e}");
    }

    #[test]
    fn gmst_matches_meeus_12b() {
        // 1987 April 10, 0h UT → 13h10m46.3668s = 197.693195°.
        let g = gmst_deg(2_446_895.5);
        assert!((g - 197.693_195).abs() < 1e-4, "gmst = {g}");
    }

    #[test]
    fn gmst_advances_one_sidereal_turn_per_day() {
        let a = gmst_deg(2_451_545.0);
        let b = gmst_deg(2_451_546.0);
        // A sidereal day is ~3m56s short of a solar day → +0.9856°/day.
        assert!((wrap180(b - a) - 0.985_647).abs() < 1e-3, "{a} → {b}");
    }

    #[test]
    fn sun_matches_meeus_25a() {
        // 1992 October 13.0 TD, JD 2448908.5. The reference values are from
        // Meeus's high-accuracy example 25.b (λ 199.90895°, R 0.99760775 AU);
        // the low-accuracy theory used here is allowed to differ, and does so
        // by 0.0001° in longitude and 5e-5 AU (8000 km, i.e. 0.005%) in radius.
        let (lambda, r) = sun_geocentric(2_448_908.5);
        assert!((lambda - 199.908_95).abs() < 0.01, "λ = {lambda}");
        assert!((r - 0.997_607_75).abs() < 1e-4, "R = {r}");
    }

    #[test]
    fn earth_distance_stays_within_the_real_orbit() {
        // Perihelion 0.9833 AU, aphelion 1.0167 AU.
        let mut lo = f64::MAX;
        let mut hi = f64::MIN;
        for d in 0..366 {
            let r = earth_heliocentric(2_451_545.0 + d as f64).len() / AU;
            lo = lo.min(r);
            hi = hi.max(r);
        }
        assert!((0.9832..0.9836).contains(&lo), "perihelion {lo}");
        assert!((1.0165..1.0169).contains(&hi), "aphelion {hi}");
    }

    #[test]
    fn moon_matches_meeus_47a_within_short_series_tolerance() {
        // 1992 April 12.0 TD → λ 133.162655°, β −3.229126°, Δ 368409.7 km.
        let (lambda, beta, dist) = moon_geocentric(2_448_724.5);
        assert!((wrap180(lambda - 133.162_655)).abs() < 0.4, "λ = {lambda}");
        assert!((beta + 3.229_126).abs() < 0.25, "β = {beta}");
        assert!((dist * 1.0e6 - 368_409.7).abs() < 3_000.0, "Δ = {} km", dist * 1.0e6);
    }

    #[test]
    fn moon_orbit_stays_physical() {
        // Perigee ≈ 356 400 km, apogee ≈ 406 700 km; |β| ≤ 5.3°; the mean
        // longitude rate is the sidereal month, 13.176°/day.
        let (mut lo, mut hi, mut max_beta) = (f64::MAX, f64::MIN, 0.0f64);
        for d in 0..800 {
            let (_, beta, dist) = moon_geocentric(2_451_545.0 + d as f64);
            lo = lo.min(dist * 1.0e6);
            hi = hi.max(dist * 1.0e6);
            max_beta = max_beta.max(beta.abs());
        }
        assert!((352_000.0..362_000.0).contains(&lo), "perigee {lo}");
        assert!((402_000.0..411_000.0).contains(&hi), "apogee {hi}");
        assert!((4.9..5.6).contains(&max_beta), "max |β| {max_beta}");

        // Mean motion over a century, so the ±0.3° periodic wobble at each
        // endpoint contributes under 0.00002°/day to the difference.
        const SIDEREAL_MONTH_DEG_PER_DAY: f64 = 13.176_358;
        let n = 36_525.0;
        let l0 = moon_geocentric(2_451_545.0).0;
        let l1 = moon_geocentric(2_451_545.0 + n).0;
        let whole = ((n * SIDEREAL_MONTH_DEG_PER_DAY - (l1 - l0)) / 360.0).round();
        let rate = ((l1 - l0) + 360.0 * whole) / n;
        // 5e-4 °/day, not tighter: the short series rounds its mean-longitude
        // coefficient to 481267.881°/century = 13.176257°/day, which is itself
        // 1e-4 short of the true sidereal rate.
        assert!((rate - SIDEREAL_MONTH_DEG_PER_DAY).abs() < 5e-4, "mean motion {rate}");
    }

    #[test]
    fn solar_disk_elements_match_meeus_29a() {
        // 1992 October 13.0 TD → P = 26.27°, B0 = 5.99°, L0 = 238.63°.
        let (p, b0, l0) = solar_p_b0_l0(2_448_908.5);
        assert!((p - 26.27).abs() < 0.02, "P = {p}");
        assert!((b0 - 5.99).abs() < 0.02, "B0 = {b0}");
        assert!((wrap180(l0 - 238.63)).abs() < 0.05, "L0 = {l0}");
    }

    /// Independent ground truth for the Carrington longitude of the disk
    /// centre, which is the easiest quantity here to get 180° wrong.
    ///
    /// NOAA SWPC's solar-region summary publishes both the Stonyhurst and the
    /// Carrington longitude of every sunspot group, and `L_carrington =
    /// L0 + L_stonyhurst_west`. Two regions from the 2026-07-24 report:
    /// `S03E14` at Carrington 99 → L0 = 99 − (−14) = 113, and `N05W46` at
    /// Carrington 159 → L0 = 159 − 46 = 113. The report's positions are for
    /// 2400 UT, i.e. 2026-07-25T00:00Z.
    #[test]
    fn l0_matches_swpc_carrington_longitudes() {
        let jd = julian_day(1_784_937_600.0); // 2026-07-25T00:00:00Z
        let (_, _, l0) = solar_p_b0_l0(jd);
        assert!(wrap180(l0 - 113.0).abs() < 1.5, "L0 = {l0}");
    }

    #[test]
    fn l0_regresses_at_the_synodic_carrington_rate() {
        // The disk centre's Carrington longitude runs backwards as the Sun
        // turns under it: one synodic Carrington rotation is 27.2753 days.
        let jd = 2_460_500.0;
        let a = solar_p_b0_l0(jd).2;
        let b = solar_p_b0_l0(jd + 1.0).2;
        assert!((wrap180(b - a) + 360.0 / 27.2753).abs() < 0.1, "{a} → {b}");
    }

    /// The vector form of the solar axis and Meeus's independent B0 formula are
    /// derived differently; if the node longitude's sign were wrong, the annual
    /// B0 curve would invert and this would fail immediately.
    #[test]
    fn solar_axis_vector_agrees_with_meeus_b0() {
        for i in 0..24 {
            let jd = 2_460_000.0 + i as f64 * 15.2;
            let (_, b0_meeus, _) = solar_p_b0_l0(jd);
            let b0_vec = sun_frame(jd).b0_deg();
            assert!((b0_vec - b0_meeus).abs() < 0.05, "jd {jd}: {b0_vec} vs {b0_meeus}");
        }
    }

    #[test]
    fn b0_peaks_in_september_and_troughs_in_march() {
        // Earth crosses the Sun's equatorial plane in early June and early
        // December; B0 reaches +7.25° around 7 September.
        let mut best = (f64::MIN, 0.0);
        let mut worst = (f64::MAX, 0.0);
        // 2024-01-01T00:00Z .. one year.
        for d in 0..366 {
            let jd = 2_460_310.5 + d as f64;
            let b0 = sun_frame(jd).b0_deg();
            if b0 > best.0 {
                best = (b0, d as f64);
            }
            if b0 < worst.0 {
                worst = (b0, d as f64);
            }
        }
        assert!((best.0 - 7.25).abs() < 0.1, "max B0 {}", best.0);
        assert!((worst.0 + 7.25).abs() < 0.1, "min B0 {}", worst.0);
        // Day 250 of 2024 is 6 September; day 64 is 4 March.
        assert!((best.1 - 250.0).abs() < 6.0, "max B0 on day {}", best.1);
        assert!((worst.1 - 64.0).abs() < 6.0, "min B0 on day {}", worst.1);
    }

    /// The sign exam for [`STONYHURST_WEST_SIGN`]: rotate a feature prograde
    /// about the solar axis for a day, then re-measure it in the *new* frame.
    /// Its longitude must have grown by the synodic rate — growing toward West
    /// is what "West-positive" means.
    #[test]
    fn stonyhurst_longitude_drifts_west() {
        for lat in [-30.0, -10.0, 0.0, 15.0, 40.0] {
            let jd = 2_460_500.0;
            let f0 = sun_frame(jd);
            let start_lon = -20.0;
            let dir0 = f0.direction(lat, start_lon);

            // One day of prograde rotation about the solar north pole.
            let n = solar_north_ecliptic(jd);
            let ang = sidereal_rotation_deg_per_day(lat).to_radians();
            let dir1 = dir0 * ang.cos()
                + n.cross(dir0) * ang.sin()
                + n * (n.dot(dir0) * (1.0 - ang.cos()));

            let f1 = sun_frame(jd + 1.0);
            let (lat1, lon1) = f1.stonyhurst_of(dir1);
            let expected = sidereal_rotation_deg_per_day(lat) - EARTH_MEAN_MOTION_DEG_PER_DAY;
            assert!((lat1 - lat).abs() < 0.05, "latitude drifted: {lat} → {lat1}");
            assert!(
                (wrap180(lon1 - start_lon) - expected).abs() < 0.05,
                "lat {lat}: drift {} expected {expected}",
                wrap180(lon1 - start_lon)
            );
        }
    }

    #[test]
    fn stonyhurst_round_trips() {
        let f = sun_frame(2_460_500.0);
        for (lat, lon) in [(0.0, 0.0), (25.0, 60.0), (-12.0, -140.0), (60.0, 179.0)] {
            let (lat2, lon2) = f.stonyhurst_of(f.direction(lat, lon));
            assert!((lat2 - lat).abs() < 1e-9 && wrap180(lon2 - lon).abs() < 1e-9);
        }
        // Longitude 0 is the sub-Earth meridian by construction.
        let (lat0, lon0) = f.stonyhurst_of(f.to_earth);
        assert!(lon0.abs() < 1e-9, "sub-Earth longitude {lon0}");
        assert!((lat0 - f.b0_deg()).abs() < 1e-9);
    }

    #[test]
    fn earth_basis_is_orthonormal_and_polar() {
        let b = earth_basis(2_460_500.3);
        for v in [b.x, b.y, b.z] {
            assert!((v.len() - 1.0).abs() < 1e-12);
        }
        assert!(b.x.dot(b.y).abs() < 1e-12 && b.x.dot(b.z).abs() < 1e-12);
        // Right-handed: x × y = z.
        assert!((b.x.cross(b.y) - b.z).len() < 1e-12);
        // The north pole sits at ecliptic latitude 90° − ε ≈ 66.56°.
        let e = obliquity_deg(2_460_500.3);
        assert!((b.z.lat_deg() - (90.0 - e)).abs() < 1e-9, "pole at {}", b.z.lat_deg());
        // ...and at ecliptic longitude 90°.
        assert!((wrap180(b.z.lon_deg() - 90.0)).abs() < 1e-9);
    }

    #[test]
    fn subsolar_point_tracks_noon() {
        // At 12:00 UTC the Sun is overhead near the Greenwich meridian; the
        // equation of time keeps it within ±4° of it all year.
        for d in 0..365 {
            let jd = julian_day(1_704_110_400.0 + d as f64 * 86_400.0); // 2024-01-01T12:00Z
            let (lat, lon) = subsolar_point(jd);
            assert!(lon.abs() < 4.5, "day {d}: subsolar longitude {lon}");
            assert!(lat.abs() < 23.5, "day {d}: subsolar latitude {lat}");
        }
        // The declination extremes are the solstices.
        let june = subsolar_point(julian_day(1_718_884_800.0)).0; // 2024-06-20T12:00Z
        let dec = subsolar_point(julian_day(1_734_696_000.0)).0; // 2024-12-20T12:00Z
        assert!((june - 23.44).abs() < 0.1, "June solstice {june}");
        assert!((dec + 23.42).abs() < 0.15, "December solstice {dec}");
    }

    #[test]
    fn subsolar_longitude_sweeps_westward_with_the_clock() {
        // Six hours later the Sun is overhead 90° further west.
        let jd = julian_day(1_704_110_400.0);
        let a = subsolar_point(jd).1;
        let b = subsolar_point(jd + 0.25).1;
        assert!((wrap180(b - a) + 90.0).abs() < 0.5, "{a} → {b}");
    }

    /// The one thing everybody knows about the Moon: the same face, always.
    #[test]
    fn the_moon_keeps_one_face_towards_the_earth() {
        for d in 0..60 {
            let jd = 2_460_500.0 + d as f64 * 0.5;
            let b = moon_basis(jd);
            for v in [b.x, b.y, b.z] {
                assert!((v.len() - 1.0).abs() < 1e-12);
            }
            assert!(b.x.dot(b.y).abs() < 1e-12 && b.x.dot(b.z).abs() < 1e-12);
            assert!((b.x.cross(b.y) - b.z).len() < 1e-12, "left-handed lunar frame");

            // Selenographic (0°, 0°) is the sub-Earth point...
            let near_side = b.apply(geodetic_to_body(0.0, 0.0));
            let to_earth = (-moon_geocentric_vec(jd)).normalize();
            assert!(near_side.dot(to_earth) > 0.999_999, "the near side has turned away");
            // ...and the far side is the far side.
            let far = b.apply(geodetic_to_body(0.0, 180.0));
            assert!(far.dot(to_earth) < -0.999_999);
        }
    }

    /// Which way round the map goes, settled by something anyone can check
    /// with binoculars: a few days after new moon the crescent lights the
    /// **eastern** limb, so Mare Crisium (17°N 59°E) catches the Sun days
    /// before Oceanus Procellarum (18°N 57°W) does.
    ///
    /// This is the test that pins the sign of `y`. Mirror the frame and every
    /// lunar feature lands on the wrong limb while the Moon still, misleadingly,
    /// keeps one face towards the Earth.
    #[test]
    fn the_waxing_crescent_lights_the_eastern_limb() {
        // Elongation 45–75° east of the Sun: three or four days old, by which
        // point the terminator has moved past Crisium.
        let jd = (0..120)
            .map(|k| 2_460_500.0 + k as f64 * 0.5)
            .find(|jd| {
                let elong = wrap180(moon_geocentric(*jd).0 - sun_geocentric(*jd).0);
                (45.0..75.0).contains(&elong)
            })
            .expect("no waxing crescent in two months");

        let b = moon_basis(jd);
        let to_sun = (-(earth_heliocentric(jd) + moon_geocentric_vec(jd))).normalize();
        let lit = |lat: f64, lon: f64| b.apply(geodetic_to_body(lat, lon)).dot(to_sun);
        assert!(lit(17.0, 59.0) > 0.15, "Mare Crisium is dark on a waxing crescent");
        assert!(lit(18.0, -57.0) < -0.15, "Oceanus Procellarum is lit on a waxing crescent");
    }

    #[test]
    fn geodetic_to_body_is_ecef() {
        // 0°N 0°E is the +X axis; 0°N 90°E is +Y; the north pole is +Z.
        assert!((geodetic_to_body(0.0, 0.0) - vec3(1.0, 0.0, 0.0)).len() < 1e-12);
        assert!((geodetic_to_body(0.0, 90.0) - vec3(0.0, 1.0, 0.0)).len() < 1e-12);
        assert!((geodetic_to_body(90.0, 0.0) - vec3(0.0, 0.0, 1.0)).len() < 1e-12);
    }
}
