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
    /// Wire protocol this family speaks. Selected by the family's `profile`
    /// key in `models.toml`; omitting it means the T-series flow, which is
    /// what every model spoke before the E-series was added.
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
    /// Wire-protocol variant: `"t-series"` (the default when absent) or
    /// `"e-series"`. See `supvan_proto::profile::PrintProfile`.
    #[serde(default)]
    profile: Option<String>,
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

/// Load the model registry from TOML. Panics if the file is not found or
/// invalid.
///
/// Must be called exactly once, before any other function in this module.
/// The default model table, baked into the binary so a `cargo install`'d
/// (or otherwise relocated) binary is self-contained. Overridden by
/// `$SUPVAN_MODELS` or a `models.toml` found on disk — see [`find_toml_path`].
const EMBEDDED_MODELS: &str = include_str!("../../../data/models.toml");

pub fn load() {
    let (contents, source) = match find_toml_path() {
        Some(path) => {
            let c = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
            (c, path)
        }
        None => (EMBEDDED_MODELS.to_string(), "<embedded>".to_string()),
    };
    let toml: ModelsToml =
        toml::from_str(&contents).unwrap_or_else(|e| panic!("failed to parse {source}: {e}"));

    let families: Vec<DriverFamily> = toml
        .families
        .iter()
        .map(|f| {
            let media_names: Vec<CString> = f
                .media_mm
                .iter()
                // PWG 5101.1 self-describing name: metric dimensions take the
                // `om_` (other-metric) class prefix; `oe_` is for inches and
                // fails the IPP Everywhere media-name regex.
                .map(|[w, h]| CString::new(format!("om_{w}x{h}mm_{w}x{h}mm")).unwrap())
                .collect();
            let media_sizes: Vec<[c_int; 2]> =
                f.media_mm.iter().map(|[w, h]| [w * 100, h * 100]).collect();

            DriverFamily {
                print_profile: profile_from_toml(f.profile.as_deref(), &f.name),
                driver_name: CString::new(f.name.as_str()).unwrap(),
                make_and_model: f.description.as_bytes().to_vec(),
                dpi: f.dpi,
                printhead_width_dots: f.printhead_dots,
                media_names,
                media_sizes,
            }
        })
        .collect();

    // Build family name → index map
    let family_index: HashMap<&str, usize> = families
        .iter()
        .enumerate()
        .map(|(i, f)| (f.driver_name.to_str().unwrap(), i))
        .collect();

    let default_family_idx = *family_index
        .get("supvan_t50")
        .expect("models.toml must define a 'supvan_t50' family");

    let models: Vec<UsbModel> = toml
        .models
        .iter()
        .map(|m| {
            assert!(
                family_index.contains_key(m.family.as_str()),
                "model '{}' references unknown family '{}'",
                m.name,
                m.family
            );
            UsbModel {
                pid: m.pid.clone(),
                name: m.name.clone(),
            }
        })
        .collect();

    // Flatten bt_patterns: (pattern, family_idx), sorted longest-first
    let mut bt_patterns: Vec<(String, usize)> = Vec::new();
    for (family_name, patterns) in &toml.bt_patterns {
        let idx = *family_index
            .get(family_name.as_str())
            .unwrap_or_else(|| panic!("bt_patterns references unknown family '{family_name}'"));
        for pattern in patterns {
            bt_patterns.push((pattern.clone(), idx));
        }
    }
    bt_patterns.sort_by_key(|p| std::cmp::Reverse(p.0.len()));

    if REGISTRY
        .set(Registry {
            families,
            models,
            bt_patterns,
            default_family_idx,
        })
        .is_err()
    {
        panic!("models::load() called more than once");
    }
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
/// before the E-series was added. Panics on an unknown value, like the rest of
/// registry loading — a family pointed at a protocol that doesn't exist would
/// otherwise print blank.
fn profile_from_toml(profile: Option<&str>, family: &str) -> PrintProfile {
    match profile {
        None | Some("t-series") => PrintProfile::TSeries,
        Some("e-series") => PrintProfile::ESeries,
        Some(other) => panic!(
            "family '{family}': unknown profile '{other}' (expected 't-series' or 'e-series')"
        ),
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
/// The distinction matters to discovery: a name that pins nothing has not
/// decided its wire protocol, whereas a name that pins a family has.
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

    /// `load()` panics if called twice, but the registry is process-wide and
    /// tests share it — initialise it exactly once, whoever gets there first.
    ///
    /// Crate-wide: every test module that touches the registry has to route
    /// through this same `Once`, or the second one to arrive panics.
    pub(crate) fn load_once() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(load);
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
    fn profile_key_defaults_to_the_t_series_flow() {
        // Omitting the key must select the flow every model spoke before the
        // E-series existed, so an untouched `models.toml` keeps working.
        for spelling in [None, Some("t-series")] {
            assert_eq!(
                profile_from_toml(spelling, "test_family"),
                PrintProfile::TSeries,
                "{spelling:?} must select the T-series flow"
            );
        }
        assert_eq!(
            profile_from_toml(Some("e-series"), "test_family"),
            PrintProfile::ESeries
        );
    }

    #[test]
    #[should_panic(expected = "unknown profile 'nonsense'")]
    fn unknown_profile_key_is_rejected_at_load_time() {
        // Silently defaulting would print blank pages on a mistyped profile.
        profile_from_toml(Some("nonsense"), "test_family");
    }
}
