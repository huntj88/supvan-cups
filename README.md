# Supvan label printer driver

A pure-Rust **IPP Everywhere** printer application for Supvan T-series thermal
label printers (and Katasymbol-branded equivalents), plus a command-line
diagnostic tool. It runs a local IPP server that CUPS — and any AirPrint /
IPP-Everywhere client — can print to driverlessly over USB or Bluetooth.

- **Full IPP Everywhere conformance** — `ipptool ipp-everywhere.test` passes
  32/0 (see [docs/CONFORMANCE.md](docs/CONFORMANCE.md)).
- **Formats**: PWG/CUPS raster and `image/jpeg` (decoded in-process).
- **USB + Bluetooth**, unified into one logical printer per device.
- **CUPS-managed** — a self-contained IPP Everywhere service: it advertises over
  DNS-SD and CUPS makes an on-demand queue (no queue to install or manage).
- **Holds jobs when the device is offline** (or jammed) and prints them on
  recovery, instead of dropping them; drops out of print dialogs when off.

Built on [`ipp-printer-app`](https://crates.io/crates/ipp-printer-app), this
project's own generic IPP-Everywhere framework. The printer protocol was
reverse-engineered from the Katasymbol Android app (v1.4.20); the repo is
`github.com/heeen/supvan-cups` (the working tree is named `katasymbol`).

## Supported printers

All USB models use VID `0x1820`; Bluetooth and USB are auto-discovered.

| Family | Models | DPI | Printhead |
|--------|--------|-----|-----------|
| T50 Series | T50M, T50M Pro, T50M Plus, T50s, T50s Pro | 203 | 48 mm / 384 dots |
| T80 Series | T80M, T80M Pro | 201 | 72 mm / 568 dots |
| G Series | G11, G15, G18, G18 Pro | 193 | 25 mm / 190 dots |
| TP76 Series | TP76I, TP76I Pro | 305 | 76 mm / 912 dots |
| TP80 Series | TP80A, TP80A Pro | 305 | 80 mm / 960 dots |
| TP86 Series | TP86A, TP86A Pro | 305 | 86 mm / 1032 dots |
| SP650 | SP650 | 203 | 48 mm / 384 dots |
| E Series | E10, E10pro, E11, E12, E16 | 203 | 12 mm / 96 dots |

The **E-series** are Bluetooth-only label makers and speak a different print
flow from the rest of the range — smaller print buffers, different `PAGE_REG`
constants and one transfer per buffer (see
[docs/E10-PROTOCOL.md](docs/E10-PROTOCOL.md)). The flow is verified against a
capture of the vendor app driving an E10pro; E11, E12 and E16 are the same
BT-only class and use it too, since the T50 flow prints them blank. Their
printheads are unconfirmed, so the whole series is driven at the E10's 12 mm —
a wider model prints correctly in a narrower band until someone captures one
([docs/E10-PROTOCOL.md](docs/E10-PROTOCOL.md) §11).
Katasymbol-branded equivalents behave as their Supvan counterparts.
The model registry lives in
[`data/models.toml`](data/models.toml) — it is compiled into the binary as a
fallback and can be overridden at runtime with `SUPVAN_MODELS` (no recompile).

Each family lists the label sizes it advertises. Continuous-tape families (the
E-series) add a `media_ladder`, which expands at load into a dense range of
requestable lengths — 4 mm to 1 m for the E10 — so a caller can ask for a
label sized to its content rather than rounding up to the nearest stock size.
Rungs must be at least 2 mm apart and a family may advertise at most 400 sizes
in total — the ladder emits every rung once per width, so it is the
width × length product that has to fit the budget. Both limits come from
measured CUPS behaviour and are explained in
[docs/E10-PROTOCOL.md](docs/E10-PROTOCOL.md) §4.1.2. Die-cut families omit the
ladder, since their stock only registers at its real sizes.

## How it works

```
discovery          IPP server (ipp-printer-app)         device (supvan-proto)
USB + BT  ──┐                                       ┌── column-major 1-bit pack
            ├─► supvan://<id> ─► Print-Job ─► print_job ─► LZMA ─► USB/BT transfer
mock://  ───┘    (mock://ID)        │  └─ image/jpeg ─► run_jpeg_job (decode→fit→dither)
                                    └──── PWG/CUPS raster ─► run_cups_raster_job
```

- **Discovery** unifies a printer's USB (hidraw) and Bluetooth (RFCOMM)
  interfaces into a single `supvan://<id>` device; `SUPVAN_MOCK=1` substitutes a
  synthetic `mock://` device.
- The **IPP server** (from `ipp-printer-app`) receives jobs; the `print_job`
  callback branches on `document-format` → `run_jpeg_job` (JPEG: decode →
  contain-fit onto the loaded label → dither) or `run_cups_raster_job`
  (PWG/CUPS raster), both feeding the `supvan-proto` pack → LZMA → transfer
  pipeline.
- We advertise over **DNS-SD** and let CUPS create a temporary on-demand queue
  (the AirPrint model) — no queue of our own. A co-resident `cups-browsed`
  should run with `OnlyUnsupportedByCUPS Yes` so it defers to CUPS rather than
  building a duplicate `implicitclass://` queue (see [docs/DEPLOY.md](docs/DEPLOY.md)).
- A **status poller** surfaces the loaded roll (`media-ready` / `media-col-ready`),
  a labels-remaining supply gauge (`printer-supply`), and error reasons — and
  drives the offline/jam behavior: when the device can't print, jobs are held
  and retried, the printer reports `stopped`, and its advert is withdrawn.

## Workspace layout

| Crate | Purpose |
|-------|---------|
| `crates/supvan-proto` | Wire protocol: USB-HID + BT-RFCOMM transports, commands, status/material parsing, bitmap packing, LZMA compression. No IPP knowledge. |
| `crates/supvan-app` | The printer application binary `supvan-printer-app`. |
| `crates/supvan-cli` | The `supvan-cli` diagnostic tool. |

The IPP/HTTP layer is the external crate **`ipp-printer-app`** (`= "0.7"`, on
crates.io) — this repo's own device-agnostic framework, not a workspace member.

## Install & run

The app needs no privileges (unprivileged port 8631; it owns no CUPS queue), so
the default is a **user-scoped** install — no sudo:

```sh
make deploy      # cargo install → ~/.cargo/bin + a user systemd unit, then start
```

Re-run `make deploy` after any change. `make uninstall-user` reverses it. For a
system-wide (FHS) install instead, `sudo make install`. Full details and the
`cups-browsed` story are in **[docs/DEPLOY.md](docs/DEPLOY.md)**; a manual CUPS
acceptance walkthrough is in [docs/CUPS_ACCEPTANCE.md](docs/CUPS_ACCEPTANCE.md).

Build prerequisites (Debian/Ubuntu): `sudo apt install pkg-config libdbus-1-dev`,
plus `bluez` for Bluetooth and `cups` at runtime. Run `make help` for all
targets (`build`, `test`, `clippy`, `lint`, `run`, …).

To run it directly without installing:

```sh
make run                          # cargo run -p supvan-app (add SUPVAN_MOCK=1 for no hardware)
# index + IPP server at http://localhost:8631/
```

## CLI tool

`supvan-cli` talks to a printer directly (bypassing CUPS) for diagnostics. Pass
a Bluetooth address or a `/dev/hidrawN` path:

```sh
supvan-cli discover                          # scan for Supvan Bluetooth devices
supvan-cli probe AA:BB:CC:DD:EE:FF           # device/status/material/version
supvan-cli material /dev/hidraw7             # loaded label + RFID + remaining
supvan-cli test-print /dev/hidraw7 --density 4
```

## Testing

```sh
cargo test --workspace        # unit + integration tests, no hardware
```

A Docker-based end-to-end test boots the app under CUPS + `cups-browsed` +
avahi and exercises discovery, `cups-browsed` coexistence, PWG-raster and JPEG
print round-trips, offline/jam hold-and-retry, and the status lifecycle — see
`ci/run-integration-test.sh`
(run by GitHub Actions alongside `cargo test`/`clippy`).

For label-free local testing, run with `SUPVAN_MOCK=1`: every page is written
as a PBM (plus a JSON manifest) under `$XDG_RUNTIME_DIR/supvan-mock/` (or
`SUPVAN_DUMP_DIR`) instead of reaching hardware. To exercise the IPP error
surface without a physical fault, layer on `SUPVAN_MOCK_FAIL=media-empty`
(single shot) or `SUPVAN_MOCK_STICKY=cover-open SUPVAN_MOCK_RECOVER_AFTER_MS=10000`
(a sticky `printer-state-reasons` that clears after 10 s). Reason tokens:
`media-empty`, `label-not-installed`, `media-jam`, `label-rw-error`,
`label-mode-error`, `ribbon-rw-error`, `ribbon-end`, `media-needed`,
`cover-open`, `head-temp-high`, `other`.

## Environment variables

| Variable | Description |
|----------|-------------|
| `SUPVAN_HOST` | Bind address (default `0.0.0.0`) |
| `SUPVAN_PORT` | IPP/HTTP port (default `8631`) |
| `SUPVAN_MODELS` | Override path to `models.toml` (else the embedded copy) |
| `SUPVAN_MOCK` | `1` runs a synthetic printer (no hardware) |
| `SUPVAN_DUMP_DIR` | Directory for debug page dumps |
| `RUST_LOG` | Log level (`debug`, `info`, `warn`, `error`) |
| `IPP_PRINTER_APP_POLL_SECS` | Status-poll cadence in seconds (default `30`) |
| `SUPVAN_MOCK_DELAY_MS` | Mock transfer delay per page (default `0`) |
| `SUPVAN_MOCK_FAIL` | Mock single-shot failure reasons (token list above) |
| `SUPVAN_MOCK_FAIL_REPEAT` | `1` re-arms `SUPVAN_MOCK_FAIL` after each use |
| `SUPVAN_MOCK_STICKY` | Mock sticky `printer-state-reasons` (same tokens) |
| `SUPVAN_MOCK_RECOVER_AFTER_MS` | Sticky reasons auto-clear after N ms |

## Documentation

- [docs/DEPLOY.md](docs/DEPLOY.md) — install, the systemd units, `cups-browsed` coexistence.
- [docs/CONFORMANCE.md](docs/CONFORMANCE.md) — the IPP Everywhere `ipptool` audit.
- [docs/CUPS_ACCEPTANCE.md](docs/CUPS_ACCEPTANCE.md) — manual CUPS acceptance walkthrough.
- [docs/PROTOCOL.md](docs/PROTOCOL.md) — the reverse-engineered Supvan wire protocol.

## License

MIT
