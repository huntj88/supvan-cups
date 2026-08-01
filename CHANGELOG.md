# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) (pre-1.0: breaking changes bump the
minor version).

## [Unreleased]

### Added

- **Support for the Bluetooth-only E-series (E10, E10pro, E11, E12, E16).**
  These label makers do not speak the T50 print flow the driver was built
  around — a job fed through it advances the tape and comes out blank. A new
  `supvan_proto::profile::PrintProfile` carries the per-model wire constants
  (4000-byte print buffers, 96-dot head, `mat=0`, constant `nodu=4`, density up
  to 19 on the leading buffer only, `BUF_FULL(0, 0)`, one LZMA stream per
  buffer, and the `0xC9`/`0xBA` commands that bracket `START_PRINT`). A family
  selects it with `profile = "e-series"` in `data/models.toml`; every other
  family keeps the T-series flow byte for byte.

  Every value was recovered by decoding a Bluetooth HCI snoop of the vendor
  Android app printing one 15 × 50 mm label. The full annotated specification,
  with the captured evidence and what was verified *unchanged*, is in
  `docs/E10-PROTOCOL.md`; `scripts/btsnoop_supvan.py` decodes such a capture.
  Only the E10/E10pro is captured; E11/E12/E16 are the same BT-only class and
  share the profile, because the alternative is the T50 flow that is known to
  print them blank. The whole series is driven at the E10's 96-dot head:
  under-declaring a wider head only narrows the printable band, while
  over-declaring it prints nothing at all (`docs/E10-PROTOCOL.md` §11).

- **BT model-name probe.** BlueZ only exposes a Supvan printer's firmware
  serial (`T0143F2408183024`), never its model. Discovery now reads
  `RD_DEV_NAME` over RFCOMM and uses the answer (`E10pro`) as the IEEE 1284
  `MDL:` field — which is what selects the driver family, and hence the wire
  protocol, for a BT-only printer.

  The probe costs an exclusive RFCOMM connection, which locks out the vendor
  app, and `libc::connect` on such a socket has no timeout of its own, so a
  printer that has since been switched off would block for the kernel's whole
  connect timeout. It is therefore bounded, run against all candidates
  concurrently, dialled on the blocking pool rather than a runtime worker, and
  spent only where it can still change the outcome — skipping any name
  `bt_patterns` already pins to the T-series flow, and any printer already in
  the persisted registry, whose discovered entry `bootstrap_printers` discards
  wholesale. Deciding that is registry policy, so it lives in the device
  backend; `discover::list_candidates` stays a probe-free BlueZ enumeration.

- **Short and arbitrary label lengths on the E-series.** `data/models.toml`
  families may now carry a `media_ladder` — `{ widths, from, steps }` — that
  expands into extra advertised lengths at load. The E10 family gets rungs from
  4 mm to 1 m, so a one-word label no longer has to round up to the 20 mm
  shortest stock size. Continuous tape has no gap to register against, so any
  length prints; the ladder exists only because CUPS can request nothing but
  the sizes the PPD enumerates.

  Two measured constraints are enforced at load: rungs must be **≥ 2 mm apart**
  (at 1 mm spacing CUPS hands the driver a raster a millimetre short of the
  requested size, clipping the label), and a family may advertise at most 400
  sizes (CUPS silently truncates the generated PPD past ~470, and a size that
  reaches a client but not the PPD crashes cups-filters' `universal` filter).
  Both are documented in `docs/E10-PROTOCOL.md` §4.1.2, along with the
  `*CustomPageSize` approach that would remove the ladder entirely once
  `ipp-printer-app` can advertise size ranges. Die-cut families (T50/T80/G/TP)
  omit the ladder, since their stock only registers at its real sizes.

- **Generic Supvan device matching.** A printer whose firmware hardware code
  isn't listed in `bt_patterns` is now discoverable via its serial-name shape
  (`[TGD]\d\d…`) inside Supvan's `A4:93:40` OUI, instead of being invisible.
  The classic-BT and BLE scanners share one implementation. Because the shape
  is deliberately loose, classic-BT auto-pairing now also skips a device that
  advertises profiles but not Serial Port, so a widened match can't turn into
  pairing prompts for hardware the driver could never talk to.

### Changed

- `Printer` owns its `PrintProfile`, set once when the transport is opened, so
  the print state machine no longer takes it as a parameter at every step.
  `Printer::print_compressed` is replaced by `Printer::print_page`, which takes
  the built buffers and performs compression itself — the page is one LZMA
  stream on the T-series flow and one per buffer on the E-series, which the
  caller shouldn't have to know. Compression runs before `CHECK_DEVICE`, so no
  LZMA pass ever falls between `START_PRINT` and the first data packet; the
  printer is live from that point and times out waiting for data.

- `PrinterStatus::has_error` / `error_description` now take a `PrintProfile`,
  because what counts as an error is model-specific: the E10pro asserts
  `ribbon_end` throughout prints the vendor app completes successfully, and
  treating it as fatal held every job with `media-needed` forever.

- `create_test_pattern` draws its per-buffer sub-patterns using the target
  profile's buffer size and margins, so the boundaries it marks are the ones
  `split_into_buffers` actually produces. It also takes the printhead width
  rather than assuming the T50's 384 dots.

- **`bt_patterns`: `e10`/`e11`/`e12`/`e16` now select `supvan_e`, not
  `supvan_t50`.** Upgrading: an E-series queue configured against an older
  release still names the `supvan_t50` driver and will keep printing blank.
  Remove and re-add it so discovery re-resolves the family.

- `models.toml` is validated rather than asserted. A `$SUPVAN_MODELS` override
  or an on-disk `models.toml` is operator-supplied, so a mistake in it (a
  duplicate media size, a ladder step CUPS cannot resolve, an unknown
  `profile`, a dangling family reference, a media dimension outside
  1..=10000 mm) is now logged and the registry falls back to the table embedded
  in the binary, instead of panicking the daemon.

### Fixed

- **24-bit sRGB rasters printed as a solid black label.** The IPP layer
  advertises `SRGB24` in its URF list, so anything that goes through
  Ghostscript — which is the normal path for a text label — arrives as 24 bpp,
  not 8-bit grey. Only the 8 bpp case was dithered; 24 bpp fell through to the
  1-bit copy path, which sized the page buffer from the *incoming*
  `bytes_per_line` (360 for a 120-dot line rather than 15) and then copied raw
  RGB into the bitmap, where white `0xFF` bytes became eight set dots. Contone
  input of any depth is now flattened to grey (Rec. 709 luma, matching the
  `image/jpeg` path so a picture prints the same either way) and dithered, and
  an unsupported depth is a clear error rather than silent ink.

- **A padded 1 bpp scanline sheared the page.** The page buffer is re-read by
  `raster_to_column_major` at a hard-coded `ceil(width/8)` stride, but it was
  allocated from the source's `bytes_per_line` whenever the input was already
  1 bpp. CUPS is free to pad a row out past the page width, and every row after
  the first then started at the wrong offset. The buffer is now packed to
  `ceil(width/8)` for every depth, and `append_line` drops the source padding
  as the row arrives.

- `center_in_printhead` now crops a page wider than the printhead
  symmetrically instead of keeping its leading bytes. The media runs centred
  under the head, so the old behaviour dropped one whole edge — the T50 family
  advertises 50 mm media on a 48 mm head, which lost 2 mm off the right rather
  than 1 mm off each side. CUPS renders the full media width because the IPP
  layer declares zero hard margins on all four sides, so the crop cannot be
  avoided upstream.

- Both the crop window and the centring offset are now measured against the
  head's *usable* width (`printhead_dots / 8 * 8`), which is what the output
  buffer holds. The G series is 190 dots, where walking the nominal width ran
  off the end of the last column: silent corruption of the next column for
  every column but the last, and an out-of-bounds panic on it.

- `supvan-cli` selects its wire protocol from `RD_DEV_NAME`, since it has no
  driver registry to consult. A failed probe is non-fatal — `RD_DEV_NAME`
  carries no name over USB HID — and falls back to the T-series flow.

## [0.5.1] - 2026-07-01

### Added

- Completed the command-opcode vocabulary in `cmd.rs` from the vendor Linux
  tool's source map (`com.supvan.supvaneditor` 1.1.4): `CHECK_RIB` (0x19),
  `RD_LAB_DPI`/`_24`/`_25` (0x22/0x24/0x25), `SET_PRTMODE` (0x33), `SEND_INF`
  (0x35), `SET_RFID_DATA` (0x5D), `TRANSFER` (0xF0, reserved/unused). These are
  constants only — we don't drive them yet (response parsing needs on-device
  verification). `docs/PROTOCOL.md` documents each plus the `FirmwareNeedUpgrade`
  status flag and the confirmation that `0xF0` is a reserved dot-pattern-transfer
  opcode (the live bitmap path stays `NEXT_ZIPPEDBULK` 0x5C).

## [0.5.0] - 2026-07-01

### Added

- **Firmware tooling (foundation).** `docs/FIRMWARE.md` documents the vendor's
  firmware check/download API (`api.supvan.com/api/upload/GetFirmwareFile`, no
  auth) and the T50-family flash protocol. New `data::build_firmware_frames` +
  `cmd::CMD_UPDATE_FW` (0xC6) provide the flash framing (`0xAA 0xC7` packets,
  reusing the print frame layout); the live flash is intentionally left to a
  caller (destructive, no on-device verification on T50). `scripts/supvan-fw-check.py`
  checks/downloads firmware for a given model + serial.

### Fixed

- `data::make_data_packet` checksum summed into `u16`, which could panic on a
  debug build for high-entropy (compressed) payloads whose byte-sum exceeds
  65535. Now sums in `u32` and truncates to the low 16 bits (matching the
  device); release-build checksum values are unchanged.

## [0.4.1] - 2026-07-01

### Fixed

- **BLE discovery no longer false-positives classic printers.** `ble_discover`
  now runs an LE-transport scan and requires the device to advertise a Supvan
  GATT service (`fee7`/`e0ff`/`ff00`) — the real BLE-print signature — instead
  of matching on name + OUI alone. Classic SPP printers (which expose only
  Serial Port `1101`) are correctly excluded, so a Classic-Bluetooth T50-series
  printer is no longer reported with `ble=true`.

## [0.4.0] - 2026-07-01

### Changed

- **BLE GATT support is now enabled by default** in `supvan-printer-app` (the
  `ble` feature is in the default set). Default builds pull `bluer` and need
  BlueZ build deps; use `--no-default-features` for a BlueZ-free build. The
  `supvan-proto` library and standalone `supvan-cli` keep BLE opt-in. The live
  BLE device path remains unverified pending E11/E12 hardware.

## [0.3.0] - 2026-06-26

Async transport stack + a feature-gated BLE GATT transport for BLE-only
printers (E11/E12-class). Verified end-to-end on a T50M Pro over USB and
Bluetooth-classic; the BLE path is implemented to the vendor spec but
unverified against hardware.

### Added

- `supvan-cli feed <target>` — advances one blank label via the `PAPER_SKIP`
  (0x2E) command (`Printer::paper_skip`).
- **BLE GATT transport** for BLE-only printers (E11/E12-class), behind the
  off-by-default `ble` feature (pulls `bluer`). BLE reuses the shared SPP codec —
  same 16-byte framing over GATT notify/write characteristics, with the vendor's
  service/characteristic auto-detect and byte-7 response correlation. Discovery
  scans for `^[TGD]\d{2}` advertisers in OUI `A4:93:40` and folds them into the
  unified `supvan://` device (USB → BT → BLE fallback). **Unverified against
  hardware** — we own no BLE printer; an E11/E12 reporter must validate it.

### Changed (breaking)

- **Transport stack is now async.** `Transport`, the new `SppPipe`/`SppCodec`
  split, and `Printer` are async (`async-trait`); blocking RFCOMM/HID FFI runs
  via `tokio::task::block_in_place`. This lets a natively-async BLE transport
  share one codec. Requires `ipp-printer-app` 0.8.0 (its `DeviceBackend`/
  `RasterDriver`/`PrintJobFn` callbacks went async; `list` now returns
  `Vec<DiscoveredDevice>`).
- Dropped the dead `Transport::raw_fd`; folded the `NEXT_ZIPPEDBULK` header
  encoding into `Transport::send_bulk_header` (was a `use_socket_io` branch).

## [0.2.0] - 2026-06-24

A cleanup, correctness, and modernization pass across all three crates
(`supvan-proto`, `supvan-app`, `supvan-cli`).

### Changed (breaking)

- **CLI: `target` is now a required positional argument** on `probe`, `material`,
  and `test-print`. The hardcoded developer Bluetooth address default was removed.
- **CLI returns proper process exit codes**: commands return `Result` and `main`
  maps failures to a single error message + exit code 1, replacing scattered
  `process::exit(1)` calls.
- **Workspace migrated to Rust edition 2024** (`resolver = "3"`); adopted let
  chains in the print/poll paths.

### Fixed

- **Reconciled the printer-status → IPP `printer-state-reasons` mapping.** Two
  divergent copies (`failure_from_status` vs `KsDevice::status`) disagreed on
  `ribbon_rw_error`, `ribbon_end`, and `head_temp_high`; they now share one
  `reasons_from_status()`, so live polling and job-failure reporting agree.
- **Print-completion timeout no longer reports success.** `print_compressed` now
  returns `Err(Error::Timeout)` instead of `Ok(())` when the 30 s completion poll
  expires; `KsJob::end` warns on timeout instead of falling through silently.
- **CLI `probe` no longer swallows transport errors** — failed queries are
  surfaced instead of being dropped by `if let Ok(Some(_))` ladders.

### Removed

- Write-only `LAST_PRINT_TIME` tracking mechanism (stored, never read).
- Unused `CMD_PAPER_SKIP` / `CMD_SET_RFID_DATA` command constants.
- Unused `log` dependency from `supvan-cli`.
- Always-empty `JobManifest.printer_name` field.
- Misleading "BCD" decode branch in the `material_probe` example.
- Tightened over-broad `pub` visibility to `pub(crate)`/private.

### Internal

- Extracted shared helpers, removing duplicated logic: `decode_status_bits`
  (BT/USB status decode), `check_header` (BT response guards), `decompress_lzma`
  (real `pub fn`, was open-coded in tests), `Printer::open_usb` / `open_bt` /
  `open_target` (collapsed five transport-construction sites), `device::open_uri`
  (one scheme dispatch for three call sites), and `dial_and_cache`.
- Named previously-bare constants (frame offsets, poll budgets, density formula,
  chunk/stride sizes, default media) and idiomatized manual loops with iterators,
  combinators, and `to_le_bytes`/`from_le_bytes`.

### Dependencies

- `cargo update`: 46 in-range patch/minor lockfile bumps.
- `toml` 0.8 → 1.0.
- Migrated `xz2` 0.1 → `liblzma` 0.4 (the maintained continuation of the same
  liblzma bindings; identical API, built from source via `cc`).

## [0.1.0]

- Initial native-Rust Supvan T50 label-printer stack: `supvan-proto` (BT/USB HID
  protocol), `supvan-app` (IPP Everywhere printer application bridging CUPS), and
  `supvan-cli` (direct diagnostic tool).
