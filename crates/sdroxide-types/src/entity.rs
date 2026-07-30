//! DXCC entity + CQ/ITU-zone resolution from a callsign, using the embedded
//! `cty.dat` country file (AD1C / country-files.com, freely redistributable
//! with attribution). Parsed once, lazily, into a longest-prefix matcher.
//!
//! Pure + wasm-safe: the data is `include_str!`'d and parsed with no I/O.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The embedded country file. Attribution: cty.dat by Jim Reisert AD1C,
/// https://www.country-files.com/ (free to redistribute).
const CTY: &str = include_str!("cty.dat");

/// Resolved entity data for a callsign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityInfo {
    /// DXCC entity name (e.g. "Germany").
    pub name: &'static str,
    pub cq_zone: u8,
    pub itu_zone: u8,
    /// Continent code (NA/SA/EU/AF/AS/OC/AN).
    pub continent: &'static str,
}

/// A DXCC entity as the country file lists it: its name, and where on the
/// planet it is. The position is the entity's nominal centre — good enough to
/// put a marker on a globe, and the only thing it is ever used for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntityPlace {
    pub name: &'static str,
    pub lat: f64,
    pub lon: f64,
    pub cq_zone: u8,
    pub continent: &'static str,
}

struct Pfx {
    key: &'static str,
    ent: usize,
    cq: u8,
    itu: u8,
    cont: &'static str,
}

struct Cty {
    /// Entity display names, indexed by `Pfx::ent`.
    entities: Vec<&'static str>,
    /// The same entities, placed — parallel to `entities`.
    places: Vec<EntityPlace>,
    /// Prefixes bucketed by first byte, each bucket sorted longest-first.
    by_first: HashMap<u8, Vec<Pfx>>,
    /// Exact full-call overrides (the `=CALL` entries).
    exact: HashMap<&'static str, Pfx>,
}

fn cty() -> &'static Cty {
    static C: OnceLock<Cty> = OnceLock::new();
    C.get_or_init(parse)
}

fn parse() -> Cty {
    let mut entities: Vec<&'static str> = Vec::new();
    let mut places: Vec<EntityPlace> = Vec::new();
    let mut by_first: HashMap<u8, Vec<Pfx>> = HashMap::new();
    let mut exact: HashMap<&'static str, Pfx> = HashMap::new();

    let lines: Vec<&str> = CTY.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // A header line is unindented and has the 8 colon-separated fields.
        if line.starts_with(char::is_whitespace) || !line.contains(':') {
            i += 1;
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 8 {
            i += 1;
            continue;
        }
        let name = fields[0].trim();
        let cq: u8 = fields[1].trim().parse().unwrap_or(0);
        let itu: u8 = fields[2].trim().parse().unwrap_or(0);
        let cont = fields[3].trim();
        // cty.dat's longitude is west-positive, the opposite of every other
        // longitude in this program.
        let lat: f64 = fields[4].trim().parse().unwrap_or(0.0);
        let lon: f64 = fields[5].trim().parse().map(|w: f64| -w).unwrap_or(0.0);
        let ent_idx = entities.len();
        entities.push(name);
        places.push(EntityPlace { name, lat, lon, cq_zone: cq, continent: cont });
        // Parse the comma-separated prefix list from the continuation lines
        // in place (each token borrows the 'static file) until one ends ';'.
        i += 1;
        while i < lines.len() {
            let l = lines[i].trim();
            let ends = l.ends_with(';');
            let l = l.trim_end_matches(';');
            for tok in l.split(',') {
                let tok = tok.trim();
                if tok.is_empty() {
                    continue;
                }
                let (exact_call, key, cqo, ituo, conto) = parse_token(tok);
                if key.is_empty() {
                    continue;
                }
                let pfx = Pfx {
                    key,
                    ent: ent_idx,
                    cq: cqo.unwrap_or(cq),
                    itu: ituo.unwrap_or(itu),
                    cont: conto.unwrap_or(cont),
                };
                if exact_call {
                    exact.insert(key, pfx);
                } else {
                    by_first
                        .entry(key.as_bytes().first().copied().unwrap_or(b'?'))
                        .or_default()
                        .push(pfx);
                }
            }
            i += 1;
            if ends {
                break;
            }
        }
    }
    // Longest-first within each bucket so the first `starts_with` is the match.
    for v in by_first.values_mut() {
        v.sort_by(|a, b| b.key.len().cmp(&a.key.len()));
    }
    Cty { entities, places, by_first, exact }
}

/// Every DXCC entity in the country file, placed. The list a "what have I not
/// worked yet" view has to be drawn against: only knowing the whole target set
/// makes the gaps in a log visible.
pub fn all_entities() -> &'static [EntityPlace] {
    &cty().places
}

/// Parse one cty.dat prefix token: an optional leading `=` (exact call), the
/// prefix/call, then optional `(cq)`, `[itu]`, `{continent}` overrides
/// (`<lat/lon>` and `~offset~` are ignored). Returns the borrowed key slice.
fn parse_token(
    tok: &'static str,
) -> (bool, &'static str, Option<u8>, Option<u8>, Option<&'static str>) {
    let exact = tok.starts_with('=');
    let body = if exact { &tok[1..] } else { tok };
    // The key is the leading run before any bracket/override char.
    let end = body.find(['(', '[', '{', '<', '~']).unwrap_or(body.len());
    let key = &body[..end];
    let mut cq = None;
    let mut itu = None;
    let mut cont = None;
    if let (Some(a), Some(b)) = (body.find('('), body.find(')')) {
        if b > a {
            cq = body[a + 1..b].trim().parse().ok();
        }
    }
    if let (Some(a), Some(b)) = (body.find('['), body.find(']')) {
        if b > a {
            itu = body[a + 1..b].trim().parse().ok();
        }
    }
    if let (Some(a), Some(b)) = (body.find('{'), body.find('}')) {
        if b > a {
            cont = Some(body[a + 1..b].trim());
        }
    }
    (exact, key, cq, itu, cont)
}

impl Cty {
    fn info(&self, p: &Pfx) -> EntityInfo {
        EntityInfo { name: self.entities[p.ent], cq_zone: p.cq, itu_zone: p.itu, continent: p.cont }
    }

    fn longest_prefix(&self, key: &str) -> Option<&Pfx> {
        let first = *key.as_bytes().first()?;
        let bucket = self.by_first.get(&first)?;
        bucket.iter().find(|p| key.starts_with(p.key))
    }
}

/// Resolve the DXCC entity + zones for a callsign. Handles `/` portable calls
/// heuristically (the shorter added part is treated as the location prefix).
pub fn resolve_callsign(call: &str) -> Option<EntityInfo> {
    let cty = cty();
    let up = call.trim().to_ascii_uppercase();
    if up.is_empty() {
        return None;
    }
    // Exact full-call override first.
    if let Some(p) = cty.exact.get(up.as_str()) {
        return Some(cty.info(p));
    }
    let key = dxcc_key(&up);
    if let Some(p) = cty.exact.get(key.as_str()) {
        return Some(cty.info(p));
    }
    cty.longest_prefix(&key).map(|p| cty.info(p))
}

/// Resolve a token meant to *be* a prefix rather than to be a callsign: an
/// exact cty.dat prefix entry, never a longest-prefix match.
///
/// The distinction is what makes a directed CQ readable. "CQ JA" names Japan
/// because `JA` is a prefix the country file lists; "CQ ZZZZ" names nothing at
/// all, and letting it fall back to whatever one-letter prefix it happens to
/// begin with would hide that call from everybody outside one country.
pub fn resolve_prefix(prefix: &str) -> Option<EntityInfo> {
    let cty = cty();
    let up = prefix.trim().to_ascii_uppercase();
    let bucket = cty.by_first.get(&up.as_bytes().first().copied()?)?;
    bucket.iter().find(|p| p.key == up).map(|p| cty.info(p))
}

/// Choose the portion of a `/`-call that identifies the DXCC entity: strip pure
/// suffixes (`/P`, `/M`, `/MM`, `/QRP`, a lone digit …), then take the shortest
/// remaining part (the location prefix) over the operator's home call.
fn dxcc_key(call: &str) -> String {
    if !call.contains('/') {
        return call.to_string();
    }
    const SUFFIXES: &[&str] = &["P", "M", "MM", "AM", "QRP", "A", "LH", "J", "R", "T"];
    let parts: Vec<&str> = call.split('/').filter(|p| !p.is_empty()).collect();
    let mut cand: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|p| {
            !(SUFFIXES.contains(p) || (p.len() == 1 && p.bytes().all(|b| b.is_ascii_digit())))
        })
        .collect();
    if cand.is_empty() {
        cand = parts;
    }
    if cand.len() == 1 {
        return cand[0].to_string();
    }
    let mut best = cand[0];
    for c in &cand[1..] {
        if c.len() < best.len() {
            best = c;
        }
    }
    best.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_common_entities() {
        assert_eq!(resolve_callsign("W1AW").unwrap().name, "United States");
        assert_eq!(resolve_callsign("DL1ABC").unwrap().name, "Fed. Rep. of Germany");
        assert_eq!(resolve_callsign("JA1XYZ").unwrap().name, "Japan");
        assert_eq!(resolve_callsign("G0ABC").unwrap().name, "England");
        assert_eq!(resolve_callsign("VK2DEF").unwrap().name, "Australia");
    }

    #[test]
    fn portable_prefix_wins() {
        // Location prefix on the /-call identifies the entity.
        assert_eq!(resolve_callsign("W1AW/KH6").unwrap().name, "Hawaii");
        assert_eq!(resolve_callsign("DL/W1AW").unwrap().name, "Fed. Rep. of Germany");
        // Pure suffixes are ignored.
        assert_eq!(resolve_callsign("G0ABC/P").unwrap().name, "England");
    }

    /// The placed list is what the award heat map is drawn from, so it has to
    /// cover the whole country file and land in the right hemispheres.
    #[test]
    fn every_entity_is_placed() {
        let all = all_entities();
        assert!(all.len() > 300, "only {} entities in the country file", all.len());
        for e in all {
            assert!((-90.0..=90.0).contains(&e.lat), "{} at latitude {}", e.name, e.lat);
            assert!((-180.0..=180.0).contains(&e.lon), "{} at longitude {}", e.name, e.lon);
        }
        let find = |name: &str| all.iter().find(|e| e.name == name).expect(name);
        // cty.dat stores west-positive longitudes; ours are east-positive, so
        // getting this backwards would put every American entity in Asia.
        let de = find("Fed. Rep. of Germany");
        assert!(de.lat > 45.0 && de.lon > 5.0 && de.lon < 20.0, "Germany at {},{}", de.lat, de.lon);
        let us = find("United States");
        assert!(us.lon < -60.0 && us.lon > -130.0, "the USA at longitude {}", us.lon);
    }

    #[test]
    fn zones_present() {
        let de = resolve_callsign("DL1ABC").unwrap();
        assert_eq!(de.cq_zone, 14);
        assert_eq!(de.continent, "EU");
    }
}
