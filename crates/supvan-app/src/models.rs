//! Central model registry: driver families, USB PIDs, media tables.
//!
//! Loaded at startup from `data/models.toml`. Call [`load()`] once before
//! accessing any other function in this module.

use std::collections::HashMap;
use std::ffi::{CString, c_int};
use std::sync::OnceLock;

use serde::Deserialize;
use supvan_proto::profile::PrintProfile;

// ---------------------------------------------------------------------------
// Public runtime types
// ---------------------------------------------------------------------------

/// A driver family groups models sharing the same printhead and DPI.
pub struct DriverFamily {
    /// Wire-protocol variant this family speaks.
    pub print_profile: PrintProfile,
    pub driver_name: CString,
    pub make_and_model: Vec<u8>,
    pub dpi: c_int,
    pub printhead_width_dots: u32,
    pub media_names: Vec<CString>,
    pub media_sizes: Vec<[c_int; 2]>,
}

/// A USB model identified by PID (VID is always 0x1820).
pub struct UsbModel {
    pub pid: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// TOML serde types (private)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct FamilyToml {
    name: String,
    description: String,
    dpi: i32,
    printhead_dots: u32,
    media_mm: Vec<[i32; 2]>,
    /// Extra requestable lengths for continuous-tape families — see
    /// [`MediaLadderToml`]. Omit for die-cut stock, where the only legal
    /// lengths are the ones the gap sensor registers against.
    #[serde(default)]
    media_ladder: Option<MediaLadderToml>,
    /// Wire-protocol variant: omit (or `"t-series"`) for the T50/T80/G/TP/SP
    /// flow, `"e-series"` for the BT-only E-series. See
    /// `supvan_proto::profile`.
    #[serde(default)]
    profile: Option<String>,
}

/// A ladder of requestable label lengths, expanded into `media_mm` at load.
///
/// Continuous tape has no fixed length, but CUPS can only ask for a size the
/// PPD enumerates, so "any length" has to be approximated by a dense list.
/// The steps coarsen as the label grows: that keeps the total under the number
/// of sizes CUPS will faithfully turn into a PPD, while holding the *relative*
/// waste roughly constant.
#[derive(Deserialize)]
struct MediaLadderToml {
    /// Tape widths the ladder applies to.
    widths: Vec<i32>,
    /// Shortest requestable length, in mm.
    from: i32,
    /// Successive `{ to, step }` bands, each running up to `to` inclusive.
    steps: Vec<LadderStepToml>,
}

#[derive(Deserialize)]
struct LadderStepToml {
    to: i32,
    step: i32,
}

impl MediaLadderToml {
    /// The closest two rungs may sit without CUPS mis-resolving the request.
    ///
    /// Measured on cups-filters 2.x: with 1 mm spacing the raster that reaches
    /// the driver is one millimetre shorter than the size the client asked for
    /// (15x20mm arrived as 152 columns, not 160), which clips the trailing
    /// edge of the label. At 2 mm every rung arrives at its nominal size.
    const MIN_STEP_MM: i32 = 2;

    /// Every `[width, length]` on the ladder, ascending by length.
    fn expand(&self, family: &str) -> Result<Vec<[i32; 2]>, String> {
        if self.from <= 0 {
            return Err(format!(
                "family '{family}': media_ladder.from must be positive"
            ));
        }
        if self.widths.is_empty() {
            return Err(format!(
                "family '{family}': media_ladder.widths is empty — a ladder with no \
                 width produces no sizes at all"
            ));
        }
        if self.steps.is_empty() {
            return Err(format!(
                "family '{family}': media_ladder.steps is empty — a ladder with no \
                 band produces no lengths at all"
            ));
        }
        // A repeat collapses when the sizes are merged, so counting it
        // against the budget below would shrink the range for nothing.
        let mut seen_widths = std::collections::HashSet::new();
        for w in &self.widths {
            if *w <= 0 {
                return Err(format!(
                    "family '{family}': media_ladder width {w} must be positive"
                ));
            }
            if !seen_widths.insert(w) {
                return Err(format!(
                    "family '{family}': media_ladder lists width {w} twice"
                ));
            }
        }
        // Each rung is emitted once per width, so it is the product that
        // has to fit the PPD budget, not the length count alone.
        let max_lengths = MAX_MEDIA_SIZES / self.widths.len();
        let mut lengths = Vec::new();
        let mut next = self.from;
        for band in &self.steps {
            if band.step < Self::MIN_STEP_MM {
                return Err(format!(
                    "family '{family}': media_ladder step {} is below the {} mm \
                     minimum — CUPS resolves closer rungs to the wrong size",
                    band.step,
                    Self::MIN_STEP_MM
                ));
            }
            // Out of order, this band would emit nothing and silently drop
            // its whole stretch of the range.
            if band.to < next {
                return Err(format!(
                    "family '{family}': media_ladder band ending at {} mm is not \
                     above the previous band — list bands in ascending order",
                    band.to
                ));
            }
            // Align to a multiple of the band's step so the rungs are round
            // numbers (…110, 115, 120, 140, 160…) rather than an offset
            // carried over from wherever the finer band happened to stop.
            let mut len = next.saturating_add((band.step - next % band.step) % band.step);
            while len <= band.to {
                // Bounded here, not just on the finished list: a `to` of a
                // few million would build it all before being rejected.
                if lengths.len() >= max_lengths {
                    return Err(format!(
                        "family '{family}': media_ladder generates more than the \
                         {MAX_MEDIA_SIZES} sizes CUPS will put in a PPD ({} widths × \
                         {max_lengths} lengths) — coarsen media_ladder.steps, lower \
                         the last band's 'to', or list fewer widths",
                        self.widths.len()
                    ));
                }
                lengths.push(len);
                len = len.saturating_add(band.step);
            }
            // Resuming past this ceiling stops a coarser band re-emitting a
            // length the finer one already covered.
            next = band.to.saturating_add(1);
        }
        Ok(self
            .widths
            .iter()
            .flat_map(|&w| lengths.iter().map(move |&h| [w, h]))
            .collect())
    }
}

#[derive(Deserialize)]
struct ModelToml {
    pid: String,
    name: String,
    family: String,
}

#[derive(Deserialize)]
struct ModelsToml {
    families: Vec<FamilyToml>,
    models: Vec<ModelToml>,
    bt_patterns: HashMap<String, Vec<String>>,
}

// ---------------------------------------------------------------------------
// Registry singleton
// ---------------------------------------------------------------------------

struct Registry {
    families: Vec<DriverFamily>,
    models: Vec<UsbModel>,
    /// (pattern, family_idx) — longest patterns first for correct matching.
    bt_patterns: Vec<(String, usize)>,
    default_family_idx: usize,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY
        .get()
        .expect("models::load() must be called before accessing the registry")
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load the model registry from TOML.
///
/// The default model table, baked into the binary so a `cargo install`'d
/// (or otherwise relocated) binary is self-contained. Overridden by
/// `$SUPVAN_MODELS` or a `models.toml` found on disk — see [`find_toml_path`].
const EMBEDDED_MODELS: &str = include_str!("../../../data/models.toml");

/// Largest `media-supported` list CUPS will faithfully turn into a PPD.
/// Measured: 999 advertised sizes produced a PPD holding 410, with the last
/// contiguous rung at 471. Stay comfortably below that.
const MAX_MEDIA_SIZES: usize = 400;

/// Largest media dimension an override may name, in millimetres. Ten metres
/// is far past any label this hardware prints, and keeps the hundredths-of-a-
/// millimetre `c_int` CUPS wants clear of overflow.
const MAX_MEDIA_MM: i32 = 10_000;

/// Load the model registry.
///
/// Must be called exactly once, before any other function in this module.
///
/// A `models.toml` found on disk or named by `$SUPVAN_MODELS` is operator-
/// supplied, so a mistake in it is logged and the registry falls back to
/// [`EMBEDDED_MODELS`] rather than killing the daemon — a typo in an override
/// should cost the operator their customisation, not their print server. Only
/// a broken *embedded* table panics, and that is a build-time bug the tests
/// catch.
pub fn load() {
    let registry = match find_toml_path() {
        Some(path) => match read_and_build(&path) {
            Ok(r) => r,
            Err(e) => {
                log::error!("models: ignoring {path}: {e}; falling back to the embedded table");
                build_registry(EMBEDDED_MODELS, "<embedded>")
                    .expect("the embedded models.toml must be valid")
            }
        },
        None => build_registry(EMBEDDED_MODELS, "<embedded>")
            .expect("the embedded models.toml must be valid"),
    };

    if REGISTRY.set(registry).is_err() {
        panic!("models::load() called more than once");
    }
}

fn read_and_build(path: &str) -> Result<Registry, String> {
    let contents = std::fs::read_to_string(path).map_err(|e| format!("read failed: {e}"))?;
    build_registry(&contents, path)
}

/// Parse and validate a `models.toml`, or describe what is wrong with it.
///
/// The returned message names only *what* is wrong, never where the table came
/// from: `load` is the one place that knows the path and the one place that
/// prints it, so prefixing here too gave operators `ignoring /path: /path: …`.
/// `source` is for the success log alone.
fn build_registry(contents: &str, source: &str) -> Result<Registry, String> {
    let toml: ModelsToml = toml::from_str(contents).map_err(|e| format!("parse failed: {e}"))?;

    let families: Vec<DriverFamily> = toml
        .families
        .iter()
        .map(|f| {
            // A repeated size would emit a duplicate `media-supported` keyword,
            // which is an IPP Everywhere conformance failure — catch the typo
            // at load time rather than in `ipptool`.
            let unique: std::collections::HashSet<&[i32; 2]> = f.media_mm.iter().collect();
            if unique.len() != f.media_mm.len() {
                return Err(format!("family '{}' lists a duplicate media size", f.name));
            }

            // Stock sizes stay first — `media-default` is entry 0 — then the
            // ladder contributes whatever lengths the stock list doesn't
            // already cover.
            let mut sizes = f.media_mm.clone();
            if let Some(ladder) = &f.media_ladder {
                let mut seen: std::collections::HashSet<[i32; 2]> =
                    unique.iter().map(|s| **s).collect();
                for size in ladder.expand(&f.name)? {
                    if seen.insert(size) {
                        sizes.push(size);
                    }
                }
            }
            // CUPS truncates the PPD it generates from `media-supported`
            // somewhere past ~470 entries, silently dropping the tail — and a
            // size a client can see but the PPD lacks crashes the `universal`
            // filter when the client asks for it.
            if sizes.len() > MAX_MEDIA_SIZES {
                return Err(format!(
                    "family '{}': {} media sizes exceeds the {MAX_MEDIA_SIZES} CUPS \
                     will put in a PPD — coarsen media_ladder.steps",
                    f.name,
                    sizes.len()
                ));
            }

            // Scaled to hundredths of a millimetre below, where a stray zero
            // would wrap `c_int` and hand CUPS a negative dimension.
            if let Some([w, h]) = sizes
                .iter()
                .find(|[w, h]| !(1..=MAX_MEDIA_MM).contains(w) || !(1..=MAX_MEDIA_MM).contains(h))
            {
                return Err(format!(
                    "family '{}': media size {w}x{h}mm is outside 1..={MAX_MEDIA_MM}mm",
                    f.name
                ));
            }

            let media_names: Vec<CString> = sizes
                .iter()
                // PWG 5101.1 self-describing name: metric dimensions take the
                // `om_` (other-metric) class prefix; `oe_` is for inches and
                // fails the IPP Everywhere media-name regex.
                .map(|[w, h]| CString::new(format!("om_{w}x{h}mm_{w}x{h}mm")).unwrap())
                .collect();
            let media_sizes: Vec<[c_int; 2]> =
                sizes.iter().map(|[w, h]| [w * 100, h * 100]).collect();

            Ok(DriverFamily {
                print_profile: profile_from_toml(f.profile.as_deref(), &f.name)?,
                driver_name: CString::new(f.name.as_str())
                    .map_err(|_| format!("family '{}': name contains a NUL byte", f.name))?,
                make_and_model: f.description.as_bytes().to_vec(),
                dpi: f.dpi,
                printhead_width_dots: f.printhead_dots,
                media_names,
                media_sizes,
            })
        })
        .collect::<Result<_, String>>()?;

    // Build family name → index map
    let family_index: HashMap<&str, usize> = families
        .iter()
        .enumerate()
        .map(|(i, f)| (f.driver_name.to_str().unwrap(), i))
        .collect();

    let default_family_idx = *family_index
        .get("supvan_t50")
        .ok_or_else(|| "must define a 'supvan_t50' family".to_string())?;

    let models: Vec<UsbModel> = toml
        .models
        .iter()
        .map(|m| {
            if !family_index.contains_key(m.family.as_str()) {
                return Err(format!(
                    "model '{}' references unknown family '{}'",
                    m.name, m.family
                ));
            }
            Ok(UsbModel {
                pid: m.pid.clone(),
                name: m.name.clone(),
            })
        })
        .collect::<Result<_, String>>()?;

    // Flatten bt_patterns: (pattern, family_idx), sorted longest-first
    let mut bt_patterns: Vec<(String, usize)> = Vec::new();
    for (family_name, patterns) in &toml.bt_patterns {
        let idx = *family_index.get(family_name.as_str()).ok_or_else(|| {
            format!("bt_patterns references unknown family '{family_name}'")
        })?;
        for pattern in patterns {
            bt_patterns.push((pattern.clone(), idx));
        }
    }
    bt_patterns.sort_by_key(|p| std::cmp::Reverse(p.0.len()));

    log::info!("models: loaded {source}");
    Ok(Registry {
        families,
        models,
        bt_patterns,
        default_family_idx,
    })
}

/// Locate a `models.toml` override on disk, or `None` to use [`EMBEDDED_MODELS`].
fn find_toml_path() -> Option<String> {
    // 1. Explicit override
    if let Ok(path) = std::env::var("SUPVAN_MODELS") {
        return Some(path);
    }

    // 2. Development / cargo run from workspace root
    // 3. System install
    let candidates = [
        "data/models.toml",
        "/usr/share/supvan-printer-app/models.toml",
    ];
    candidates
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// All driver families.
pub fn families() -> &'static [DriverFamily] {
    &registry().families
}

/// The default driver family (supvan_t50).
pub fn default_family() -> &'static DriverFamily {
    &registry().families[registry().default_family_idx]
}

/// Find a USB model by its PID string (lowercase hex, e.g. `"2073"`).
pub fn model_by_pid(pid: &str) -> Option<&'static UsbModel> {
    registry()
        .models
        .iter()
        .find(|m| m.pid.eq_ignore_ascii_case(pid))
}

/// Map a family's `profile` string from `models.toml` onto a [`PrintProfile`].
///
/// Omitting the key means the T-series flow, which is what every model spoke
/// before the E-series was added.
fn profile_from_toml(profile: Option<&str>, family: &str) -> Result<PrintProfile, String> {
    match profile {
        None | Some("t-series") => Ok(PrintProfile::TSeries),
        Some("e-series") => Ok(PrintProfile::ESeries),
        Some(other) => Err(format!(
            "family '{family}': unknown profile '{other}' (expected 't-series' or 'e-series')"
        )),
    }
}

/// Look up a driver family by its registry name (`driver_name`).
pub fn family_by_name(name: &str) -> Option<&'static DriverFamily> {
    registry()
        .families
        .iter()
        .find(|f| f.driver_name.to_string_lossy() == name)
}

/// The wire protocol a driver family speaks. The single place a `driver_name`
/// is turned into a [`PrintProfile`]; an unknown name falls back to the
/// T-series flow, which is what every model did before the E-series.
pub fn profile_for_driver(driver_name: &str) -> PrintProfile {
    family_by_name(driver_name)
        .map(|f| f.print_profile)
        .unwrap_or_default()
}

/// The driver family a name pins through `bt_patterns`, or `None` when nothing
/// matches.
///
/// The distinction matters to discovery: a name that pins nothing reached us
/// through the generic serial-name fallback and its model is still unknown,
/// whereas a name that pins a family has already decided the wire protocol.
pub fn family_from_bt_patterns(name: &str) -> Option<&'static DriverFamily> {
    let lower = name.to_lowercase();
    let reg = registry();
    reg.bt_patterns
        .iter()
        .find(|(pattern, _)| lower.contains(pattern.as_str()))
        .map(|(_, idx)| &reg.families[*idx])
}

/// Determine the driver family from a model name or BT broadcast name.
///
/// Uses substring matching against bt_patterns (longest first).
/// Falls back to the default family for unknown names.
pub fn family_for_model_hint(name: &str) -> &'static DriverFamily {
    let reg = registry();
    family_from_bt_patterns(name).unwrap_or(&reg.families[reg.default_family_idx])
}

/// Check if a Bluetooth device name matches any known Supvan printer pattern.
pub fn is_matching_bt_name(name: &str) -> bool {
    let lower = name.to_lowercase();

    if lower.contains("supvan") || lower.contains("katasymbol") {
        return true;
    }

    registry()
        .bt_patterns
        .iter()
        .any(|(pattern, _)| lower.contains(pattern.as_str()))
}

/// Supvan's assigned MAC OUI. Both the classic (BR/EDR) and LE addresses of
/// every observed printer sit in this block.
pub fn is_supvan_oui(addr: &str) -> bool {
    addr.get(..8)
        .is_some_and(|oui| oui.eq_ignore_ascii_case("A4:93:40"))
}

/// True for the firmware *serial name* broadcast when a printer advertises no
/// marketing name: a `T`/`G`/`D` family letter, a hardware code, then the unit
/// serial (`T0143F2408183024` for an E10, `T0117A2410211517` for a T50M Pro).
///
/// [`bt_patterns`](is_matching_bt_name) can only list codes we have physically
/// seen, so genuine unknown units would be invisible to discovery. Safe as a
/// generic fallback when paired with [`is_supvan_oui`] and an SPP UUID.
pub fn is_supvan_serial_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 3
        && matches!(b[0], b'T' | b'G' | b'D')
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
}

/// Whether a Bluetooth device looks like a Supvan printer, given both its
/// address and advertised name. Accepts known name patterns anywhere, plus
/// unknown firmware serial names inside the Supvan OUI.
pub fn is_matching_bt_device(addr: &str, name: &str) -> bool {
    is_matching_bt_name(name) || (is_supvan_oui(addr) && is_supvan_serial_name(name))
}

/// Parse the MDL field from an IEEE 1284 device ID string.
///
/// Example: `"MFG:Supvan;MDL:T50M Pro;CMD:SUPVAN;"` → `Some("T50M Pro")`
pub fn parse_mdl(device_id: &str) -> Option<&str> {
    device_id
        .split(';')
        .find_map(|field| field.strip_prefix("MDL:"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn accepts_unknown_hardware_code_serial_names() {
        // E10 (reporter's unit) and T50M Pro reference unit.
        assert!(is_supvan_serial_name("T0143F2408183024"));
        assert!(is_supvan_serial_name("T0117A2410211517"));
        assert!(is_supvan_serial_name("G15Mini"));
    }

    #[test]
    fn rejects_non_printer_serial_names() {
        assert!(!is_supvan_serial_name("Some Headphones"));
        assert!(!is_supvan_serial_name("TX"));
        assert!(!is_supvan_serial_name("X12foo"));
    }

    #[test]
    fn oui_gate_is_case_insensitive_and_bounded() {
        assert!(is_supvan_oui("A4:93:40:5D:71:B6"));
        assert!(is_supvan_oui("a4:93:40:5d:71:b6"));
        assert!(!is_supvan_oui("00:11:22:33:44:55"));
        assert!(!is_supvan_oui("A4:93"));
        // Long enough in bytes, but byte 8 lands mid-character: a public
        // helper must not panic on whatever string a caller hands it.
        assert!(!is_supvan_oui("A4:93:4Ø:5D"));
    }

    #[test]
    fn serial_name_fallback_needs_the_supvan_oui() {
        load_once();
        // A generic `[TGD]\d\d` name from someone else's vendor must not be
        // adopted and auto-paired.
        assert!(!is_matching_bt_device("00:11:22:33:44:55", "T99 Speaker"));
        assert!(is_matching_bt_device("A4:93:40:5D:71:B6", "T99 Speaker"));
    }

    fn ladder(from: i32, steps: &[(i32, i32)]) -> Result<Vec<i32>, String> {
        let l = MediaLadderToml {
            widths: vec![15],
            from,
            steps: steps
                .iter()
                .map(|&(to, step)| LadderStepToml { to, step })
                .collect(),
        };
        Ok(l.expand("test")?.into_iter().map(|[_, h]| h).collect())
    }

    #[test]
    fn ladder_bands_do_not_overlap_or_repeat() {
        let lengths = ladder(4, &[(10, 2), (30, 5), (100, 20)]).unwrap();
        assert_eq!(lengths, vec![4, 6, 8, 10, 15, 20, 25, 30, 40, 60, 80, 100]);
    }

    #[test]
    fn ladder_rungs_are_multiples_of_their_step() {
        // A band picks up at the next multiple of its own step, not at
        // wherever the finer band below it stopped.
        let lengths = ladder(4, &[(60, 2), (120, 5), (300, 20)]).unwrap();
        assert!(lengths.contains(&60), "fine band runs to its ceiling");
        assert_eq!(lengths.iter().find(|&&h| h > 60), Some(&65));
        assert_eq!(lengths.iter().find(|&&h| h > 120), Some(&140));
        assert!(lengths.contains(&300), "coarse band reaches its ceiling");
        assert!(
            lengths.windows(2).all(|w| w[0] < w[1]),
            "strictly ascending"
        );
    }

    #[test]
    fn ladder_covers_every_width() {
        let l = MediaLadderToml {
            widths: vec![12, 15],
            from: 4,
            steps: vec![LadderStepToml { to: 8, step: 2 }],
        };
        assert_eq!(
            l.expand("test").unwrap(),
            vec![[12, 4], [12, 6], [12, 8], [15, 4], [15, 6], [15, 8]]
        );
    }

    #[test]
    fn ladder_rejects_rungs_cups_cannot_resolve() {
        // 1 mm spacing makes CUPS hand the driver a raster a millimetre short
        // of the requested size, clipping the label.
        let e = ladder(4, &[(20, 1)]).unwrap_err();
        assert!(e.contains("below the 2 mm minimum"), "{e}");
    }

    #[test]
    fn ladder_rejects_an_unbounded_range_before_expanding_it() {
        // An operator typo of a kilometre-long tape must fail fast, not
        // allocate a million rungs and only then be rejected.
        let e = ladder(4, &[(1_000_000, 2)]).unwrap_err();
        assert!(e.contains("coarsen"), "{e}");
        let e = ladder(4, &[(i32::MAX, 2)]).unwrap_err();
        assert!(e.contains("coarsen"), "{e}");
    }

    /// Bounding lengths alone let the cross-product reach `widths x 400`.
    #[test]
    fn ladder_bounds_the_width_by_length_product_not_just_the_lengths() {
        let wide = MediaLadderToml {
            widths: (1..=40).collect(),
            from: 4,
            steps: vec![LadderStepToml { to: 1_000, step: 2 }],
        };
        let e = wide.expand("test").unwrap_err();
        assert!(e.contains("40 widths"), "{e}");

        // Same ladder, one width: comfortably inside the budget.
        let narrow = MediaLadderToml {
            widths: vec![15],
            from: 4,
            steps: vec![LadderStepToml { to: 200, step: 2 }],
        };
        let sizes = narrow.expand("test").expect("one width fits");
        assert!(sizes.len() <= MAX_MEDIA_SIZES, "{} sizes", sizes.len());

        // Whatever a ladder does return stays inside the budget.
        let budget = MediaLadderToml {
            widths: vec![12, 15, 20, 25],
            from: 4,
            steps: vec![LadderStepToml { to: 1_000, step: 2 }],
        };
        if let Ok(sizes) = budget.expand("test") {
            assert!(sizes.len() <= MAX_MEDIA_SIZES, "{} sizes", sizes.len());
        }
    }

    /// A repeat is merged away later, so counting it against the budget
    /// would shrink the length range for nothing.
    #[test]
    fn ladder_rejects_a_repeated_width() {
        let l = MediaLadderToml {
            widths: vec![15, 15],
            from: 4,
            steps: vec![LadderStepToml { to: 60, step: 2 }],
        };
        assert!(l.expand("test").unwrap_err().contains("twice"));
    }

    #[test]
    fn ladder_rejects_a_non_positive_width() {
        let l = MediaLadderToml {
            widths: vec![0],
            from: 4,
            steps: vec![LadderStepToml { to: 60, step: 2 }],
        };
        assert!(l.expand("test").unwrap_err().contains("must be positive"));
    }

    /// Either half being empty expands to no sizes at all, which would
    /// silently contribute nothing instead of flagging a useless ladder.
    #[test]
    fn ladder_rejects_an_empty_width_or_step_list() {
        let l = MediaLadderToml {
            widths: vec![],
            from: 4,
            steps: vec![LadderStepToml { to: 60, step: 2 }],
        };
        assert!(l.expand("test").unwrap_err().contains("widths is empty"));

        let l = MediaLadderToml {
            widths: vec![15],
            from: 4,
            steps: vec![],
        };
        assert!(l.expand("test").unwrap_err().contains("steps is empty"));
    }

    #[test]
    fn ladder_rejects_bands_that_are_out_of_order() {
        // The second band ends below where the first stopped, so it would
        // contribute nothing and the 20-60 mm stretch would vanish silently.
        let e = ladder(4, &[(60, 2), (20, 5)]).unwrap_err();
        assert!(e.contains("list bands in ascending order"), "{e}");
    }

    /// A bad operator-supplied table must be rejected with a message rather
    /// than taking the daemon down; `load()` then falls back to the embedded
    /// one.
    #[test]
    fn a_broken_override_is_an_error_not_a_panic() {
        // A minimal but valid table, then one mistake at a time on top of it.
        // Top-level keys come first: anything after `[[families]]` belongs to
        // that family.
        let base = "models = []\n\n[bt_patterns]\n\n\
                    [[families]]\nname = \"supvan_t50\"\ndescription = \"T50\"\n\
                    dpi = 203\nprinthead_dots = 384\nmedia_mm = [[50, 30]]\n"
            .to_string();
        assert!(
            build_registry(&base, "test").is_ok(),
            "the base table is valid"
        );

        for (toml, expected) in [
            (format!("{base}profile = \"nonsense\"\n"), "unknown profile"),
            (
                base.replace("supvan_t50", "supvan_other"),
                "must define a 'supvan_t50' family",
            ),
            (
                base.replace(
                    "models = []",
                    "models = [{ pid = \"2073\", name = \"X\", family = \"nope\" }]",
                ),
                "unknown family",
            ),
            (
                base.replace("[bt_patterns]", "[bt_patterns]\nnope = [\"t99\"]"),
                "bt_patterns references unknown family",
            ),
            (
                base.replace("media_mm = [[50, 30]]", "media_mm = [[50, 30], [50, 30]]"),
                "duplicate media size",
            ),
            (
                format!(
                    "{base}media_ladder = {{ widths = [15], from = 4, \
                     steps = [{{ to = 20, step = 1 }}] }}\n"
                ),
                "below the 2 mm minimum",
            ),
            ("not toml at all {{{".to_string(), "parse failed"),
            (
                base.replace("media_mm = [[50, 30]]", "media_mm = [[50, 30000000]]"),
                "outside 1..=10000mm",
            ),
            (
                base.replace("media_mm = [[50, 30]]", "media_mm = [[0, 30]]"),
                "outside 1..=10000mm",
            ),
        ] {
            let e = match build_registry(&toml, "test") {
                Err(e) => e,
                Ok(_) => panic!("expected {expected:?} to be rejected"),
            };
            assert!(e.contains(expected), "expected {expected:?} in {e:?}");
        }
    }

    /// `load()` panics if called twice, but the registry is process-wide and
    /// tests share it — initialise it exactly once, whoever gets there first.
    ///
    /// Crate-wide: every test module that touches the registry has to route
    /// through this same `Once`, or the second one to arrive panics.
    pub(crate) fn load_once() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(load);
    }

    /// `load` already prefixes the path, so naming it here too showed
    /// operators `ignoring /path: /path: ...`.
    #[test]
    fn registry_errors_do_not_name_their_source() {
        let toml = "models = []\n\n[bt_patterns]\n\n\
                    [[families]]\nname = \"supvan_other\"\ndescription = \"X\"\n\
                    dpi = 203\nprinthead_dots = 384\nmedia_mm = [[50, 30]]\n";
        let e = match build_registry(toml, "/etc/supvan/models.toml") {
            Err(e) => e,
            Ok(_) => panic!("expected the table to be rejected"),
        };
        assert!(
            !e.contains("/etc/supvan/models.toml"),
            "message names its source: {e:?}"
        );
    }

    /// The table compiled into the binary is the fallback for every bad
    /// override, so it has to be valid unconditionally.
    #[test]
    fn the_embedded_table_is_valid() {
        build_registry(EMBEDDED_MODELS, "<embedded>").expect("embedded models.toml");
    }

    #[test]
    fn e_series_family_speaks_the_e_profile_on_its_own_head() {
        load_once();
        let f = family_by_name("supvan_e").expect("e-series family");
        assert_eq!(f.print_profile, PrintProfile::ESeries);
        // The E-series has exactly one head width, so the registry and the
        // profile's built-in default must not drift apart. (`TSeries` spans
        // 384/640/960-dot heads, so no such invariant exists there.)
        assert_eq!(
            f.printhead_width_dots,
            PrintProfile::ESeries.params().default_printhead_dots
        );
    }

    /// The chain that decides a BT-only printer's wire protocol, end to end:
    /// `RD_DEV_NAME` -> `MDL:` -> driver family -> profile. Every link is
    /// load-bearing — if `probe_bt_model_name` can't supply the model, the
    /// `MDL:` field falls back to "T50 Series" and an E10 silently prints
    /// blank on the T-series flow.
    #[test]
    fn probed_model_name_routes_an_e10_onto_the_e_series_profile() {
        load_once();
        let device_id = format!("MFG:Supvan;MDL:{};CMD:SUPVAN;", "E10pro");
        let mdl = parse_mdl(&device_id).expect("MDL field");
        assert_eq!(mdl, "E10pro");

        let family = family_for_model_hint(mdl);
        assert_eq!(family.driver_name.to_str().unwrap(), "supvan_e");
        assert_eq!(
            profile_for_driver(&family.driver_name.to_string_lossy()),
            PrintProfile::ESeries
        );

        // The serial name BlueZ advertises must route the same way, since
        // `t0143` is listed against the E-series family too.
        assert_eq!(
            family_for_model_hint("T0143F2408183024")
                .driver_name
                .to_str()
                .unwrap(),
            "supvan_e"
        );

        // The rest of the series rides on the E10 capture rather than the T50
        // flow, which prints them blank.
        for name in ["E11", "E12", "E16"] {
            let f = family_for_model_hint(name);
            assert_eq!(
                f.driver_name.to_str().unwrap(),
                "supvan_e",
                "{name} must not fall back to the T50 driver"
            );
            assert_eq!(
                profile_for_driver(&f.driver_name.to_string_lossy()),
                PrintProfile::ESeries
            );
        }

        // A BLE-only unit can't be probed over RFCOMM, so it arrives as the
        // placeholder `E-Series` — which must still reach the E flow.
        assert_eq!(
            family_for_model_hint("E-Series")
                .driver_name
                .to_str()
                .unwrap(),
            "supvan_e"
        );

        // And the failure mode this guards: no probe, no E-series profile.
        assert_eq!(
            profile_for_driver(
                &family_for_model_hint("T50 Series")
                    .driver_name
                    .to_string_lossy()
            ),
            PrintProfile::TSeries
        );
    }

    #[test]
    fn e_series_ladder_fits_in_a_ppd_and_keeps_its_stock_default() {
        load_once();
        let f = family_by_name("supvan_e").expect("e-series family");
        assert!(
            f.media_names.len() <= MAX_MEDIA_SIZES,
            "{} sizes would be truncated by CUPS",
            f.media_names.len()
        );
        // `media-default` is entry 0: it must stay a real stock size, not the
        // shortest rung the ladder starts at.
        assert_eq!(f.media_names[0].to_str().unwrap(), "om_12x20mm_12x20mm");
        let names: Vec<&str> = f.media_names.iter().map(|n| n.to_str().unwrap()).collect();
        assert!(names.contains(&"om_15x4mm_15x4mm"), "short rung advertised");
        assert!(names.contains(&"om_15x50mm_15x50mm"), "stock size kept");
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "ladder duplicated a stock size");
    }

    #[test]
    fn die_cut_families_get_no_ladder() {
        load_once();
        // The T50 runs gap-registered stock: only sizes the sensor can find.
        let f = family_by_name("supvan_t50").expect("t50 family");
        assert_eq!(f.print_profile, PrintProfile::TSeries);
        let names: Vec<&str> = f.media_names.iter().map(|n| n.to_str().unwrap()).collect();
        assert!(!names.contains(&"om_15x4mm_15x4mm"));
    }

    #[test]
    fn profile_key_defaults_to_the_t_series_flow() {
        // Omitting the key must select the flow every model spoke before the
        // E-series existed, so an untouched `models.toml` keeps working.
        for spelling in [None, Some("t-series")] {
            assert_eq!(
                profile_from_toml(spelling, "test_family").unwrap(),
                PrintProfile::TSeries,
                "{spelling:?} must select the T-series flow"
            );
        }
        assert_eq!(
            profile_from_toml(Some("e-series"), "test_family").unwrap(),
            PrintProfile::ESeries
        );
    }

    #[test]
    fn unknown_profile_key_is_rejected_at_load_time() {
        // Silently defaulting would print blank pages on a mistyped profile.
        let e = profile_from_toml(Some("nonsense"), "test_family").unwrap_err();
        assert!(e.contains("unknown profile 'nonsense'"), "{e}");
    }
}
