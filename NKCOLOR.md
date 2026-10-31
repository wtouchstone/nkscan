# nkcolor fork changes

This file describes every local change from upstream nkscan. Branch `main`
holds 31 commits on top of upstream `v0.17.0` (`0857681`). Each commit subject
starts with "nkcolor:".

The 2026-10-27 sync to 0.17.0 is in [Upstream reconciliation](#upstream-reconciliation).
The 2026-10-09 rebase onto `942121c` is in
[docs/UPSTREAM_AUDIT_2026-10-09.md](docs/UPSTREAM_AUDIT_2026-10-09.md). Frame
placement follows upstream from that date: `scan_frame` scans the rectangle it
is given.

`nkscan` controls the scanner and records acquisition evidence. `nkcolor`
packages that evidence and reconstructs Nikon's image. The fork does not put
the color reconstruction inside the scanner driver.

Upstream 0.17 made the Rust session API async. Every fork function that talks
to the scanner is now `async fn` too: `Session::setup_record`,
`Session::calibrate_shading`, `Session::autoexpose_frame_evidence_with`,
`Session::autoexpose_with_evidence`, and `focus::focus_in_table`. The command
line and Python call them through `block_on`, as upstream does. The command
line and Python interfaces do not change for this.

## Command-line acquisition

Files: `src/bin/nkscan/cli.rs`, `src/bin/nkscan/scan.rs`,
`src/bin/nkscan/io.rs`, `src/bin/nkscan/clean.rs`,
`src/bin/nkscan/calibrate.rs`, and `src/bin/nkscan/main.rs`.

| Change | Interface | Reason |
| --- | --- | --- |
| Save meter evidence | `scan --meter` | Keep each meter image, its exposure, the rectangle metering measured in it, the selected scan exposure, each SET WINDOW record, and the scanner setup records. |
| Use the verified Nikon negative meter | `scan --nikon-meter` (requires `--meter` and `--film negative`) | Meter as a color negative with upstream's Nikon Scan port, and refuse a scanner that no Nikon builder record verifies. Write `method=nikon-negative` in the meter record. |
| Use known exposures | `scan --exposure RED GREEN BLUE` | Repeat a measured acquisition without another meter pass. |
| Scan at Nikon's LS-8000 exposures | `scan --nikon-exposures` (conflicts with `--exposure`) | Turn off [own film base exposure](#ls-8000-own-film-base-exposure) and scan an LS-8000 color negative at Nikon Scan's exposures. |
| Select focus | `scan --focus X Y` and `scan --focus-position POSITION` | Use a point inside the frame or a known absolute lens position. |
| Refocus before the scan | `scan --refocus-before-scan` | Run autofocus after metering and immediately before the full pass. |
| Reuse detected rectangles | repeatable `scan --frame-rect-for FRAME:TOP:LEFT:BOTTOM:RIGHT` | Avoid another strip thumbnail and frame search. Use `--frames` to select entries. Upstream's `frame::at_frame` registers each rectangle against the table the unit holds. Metering uses the film base from `--base-density`, or Nikon Scan's default base density without it. |
| Meter with the thumbnail's film base | `scan --base-density RED GREEN BLUE` (requires `--frame-rect-for`) | A run that reuses rectangles takes no thumbnail, so it metered against Nikon Scan's default base. The Web UI passes the base that its thumbnail pass measured (`Discovery.base_density`), so metering matches upstream, which meters every frame against the base its own thumbnail measured. Each value is a finite density from 0 to 4. Reason: on a Fuji roll developed in ECN-2, the default base gave red 9% and blue 28% less light than the measured base, and the raw film base read green (2026-10-29). Test: `cli::tests::a_reused_thumbnail_carries_its_film_base`. |
| Move a frame | `--frame-offset X Y` and repeatable `--frame-offset-for FRAME:X:Y` | Move the rectangle by the user's signed adjustment. Upstream scans the moved rectangle as given. |
| Overscan a negative | `scan --overscan UNITS` | Expand only the final pass on scanners that address the film directly (`framing::overscan_rect`). Focus and metering use the frame. Negative film only. |
| Capture a Super Fine pair | `scan --superfine-pair` | Scan the same frame in the normal mode and in Super Fine, with the same window, exposure and focus. See `docs/SUPERFINE_PAIR_CAPTURE.md`. |
| Clean a saved scan | `nkscan clean` | Run the dust processor on a saved RGB TIFF and its infrared TIFF without a scanner. |
| Choose the cleaning quality | `scan --clean-quality normal\|fine` (requires `--clean`) | Nikon's Normal or Fine reconstruction. |
| Emit machine progress | `NKSCAN_PROGRESS_LINES=1` | Give the Web UI stable `NKSCAN_PROGRESS frame= phase= bytes= total=` records without parsing progress-bar drawing. |
| Log submitted geometry | `NKSCAN_FRAME_REQUEST` and `NKSCAN_FRAME_POSITION` | Trace the detected rectangle, user adjustment, submitted rectangle, the full pass rectangle, and whether the pass completed. |
| Log the scanner lifecycle | `nkscan started`, `nkscan finished` and `nkscan failed` lines; session open and close lines | Show which nkscan process held the scanner and whether it ended. On Windows, log driver refusals, bus faults, slow commands, and every 30 s while a command still waits on `scsiscan.sys`. |
| Use the complete thumbnail height | automatic thumbnail window setting (`thumbnail::strip_height`) | When an LS-4x/LS-5x strip adapter reports a zero Y maximum before discovery, use the documented `(maximum frame count + 2) × gate length` transport span. |
| Calibrate on request | `nkscan calibrate`, Python `Session.calibrate_shading()` and `Capabilities.calibrate` | Nikon Scan's Calibrate button: the scanner measures its light source again, for streaks along the film travel. Nikon never sends it before a scan, so nkscan does not either. |
| Clean with a verified profile | `clean::profile_for` | The LS-8000 takes the medium-format profile it shares with the LS-9000. An unrecognized scanner is refused instead of cleaned with the LS-9000 profile. |
| Make cleaning and infrared retention exclusive | `scan --clean` conflicts with `scan --ir`; the Python `scan_frame` call rejects both together | Cleaning uses infrared internally. A later clean requests `--ir` without `--clean` and stores RGBI in nkcolor. |
| Ignore false LS-8000 AE bits | `autoexpose::unit_autoexposure_kind` | The LS-8000 reports reserved D1h bits as AE and AE-with-white-balance support. Nikon Scan uses ordinary image passes. The fork forces host metering so it keeps the meter pixels. |
| Reject incomplete frame output | `Error::IncompleteScan`; scan command exit failure | Do not turn a short scanner pass into a full-size TIFF with empty or repeated data. |
| Treat FH-835M and FH-835S as 135 | `FilmFormat::from_holder(0x14 \| 0x15)` | The LS-9000 interface document lists both as 35 mm holders. Upstream offers them the 120 choices. |
| Log the SET WINDOW records, focus mode and open failures | `NKSCAN_SET_WINDOW` lines; focus-mode lines; `device::probe` debug lines | Compare a run with a Nikon Scan capture without a USB trace. Show which focus a pass used. Say why a scanner path did not open. |

### Meter record

The meter record is `<basename>_<n>_meter.txt`, `format=nkscan-meter-v2`. The
lines are, in order:

- `format=nkscan-meter-v2`
- `method=nikon-negative` (with `--nikon-meter`) or `method=generic`
- `startup=red:R,green:G,blue:B`: the start-up exposures that seeded the meter
- for each pass: `pass=N dpi= rows= cols= bits= channels=[...] complete= exposures=red:R,green:G,blue:B`
- for each pass, new in this sync: `analyzer_rect=N LINE0,PIXEL0,LINE1,PIXEL1`
- `selected=red:R,green:G,blue:B`: the full-scan exposures
- `nikon_final=red:R,green:G,blue:B`, new on 2026-10-31: Nikon Scan's
  full-scan exposures. They differ from `selected` only on an LS-8000 color
  negative, which is exposed to each channel's own film base. Written whenever
  host metering ran. An older reader skips the line; a record without it was
  scanned at Nikon's exposures.
- the setup records before and after the scan

The files also include the meter TIFF files (`_meter_N.tiff`), the SET WINDOW
records of each meter pass and of the full scan, and the raw setup records.

`analyzer_rect` is the rectangle upstream's metering measured in that pass:
first line, first pixel, last line, last pixel, inclusive. Lines run along the
feed. Pixels run along the sensor from the far end. This is the orientation of
Nikon Scan's builder image, and of nkcolor's `nikon_analyzer_rect`. The line is
new because upstream now chooses the rectangle from the adapter, the holder,
the film and the format. nkcolor can no longer compute it from the image size.
An older reader skips the line.

`method=generic` now means upstream's metering for the chosen `--film`. It is
Nikon Scan's rule for every film, but no Nikon builder record verifies it on
that scanner, so nkcolor must not treat it as complete Nikon evidence.

## Nikon negative meter

Files: `src/scan/meter/ls8000.rs`, `src/scan/meter/ls8000-characteristic.bin`,
`src/scan/meter/mod.rs`, `src/scan/meter/negative.rs`, `src/scan/frame.rs`, and
`tests/data/nikon-builder-*.bin`.

Upstream 0.13 ported Nikon Scan's metering: `CNkIPPrescan::Execute` (the
prescan sequence) and `CNegaAlgorithm::Process` (the negative analyzer and
film curve), from LS5000.md3 and LS9000.md3. The fork's own analyzer did the
same work for the LS-50/LS-5000. On the fork's LS-5000 builder record of
2026-09-02, upstream's port gives Nikon's three exposures (100454, 197670,
244673) to the count. The fork's LS-50/LS-5000 analyzer, its four-pass ladder
and `Metering::nikon_negative` are therefore dropped. Upstream's
`Metering::seed` and its clipped-channel table are the same constants as the
fork's ladder.

The LS-8000 runs LS8000.md3. Upstream uses the LS-9000 curve on every
medium-format scanner. On the fork's LS-8000 builder record of 2026-09-10,
that gives the right red exposure and the wrong green and blue (879020,
2322125, 3178074 against Nikon's 879020, 3701152, 4671874). `meter::ls8000`
adds the LS-8000 form, which direct execution of LS8000.md3 showed:

- its own 1,024-byte characteristic resource (the film curve);
- a neutral ratio is valid between 0.125 and 8, and both ratios of a channel
  must be valid;
- the balanced shadow end counts its tail over every pixel of the crop;
- the highlight weight is the smallest channel weight off the shifted curve;
- `AutoFilmMode` weights by the least dense channel.

Upstream's `Metering::step` calls `ls8000::exposures` for a color negative on
an LS-8000, and `negative::exposures` everywhere else. This applies to every
LS-8000 color-negative metering, with or without `--nikon-meter`.

`meter::nikon_negative_supported` names the units a Nikon builder record
verifies: LS-50, LS-5000 and LS-8000. `frame::Options::nikon_negative_meter`
(`scan --nikon-meter`, `scan_frame(nikon_negative_meter=True)`) refuses every
other unit. It meters as a color negative whatever `--film` or `film=` says, and
per channel (white balance unlocked), as Nikon Scan meters a negative.

The measured rectangle (the crop) is upstream's: from the adapter, the holder
and the film on a 35 mm unit, and from the format on the LS-8000 and LS-9000.
The fork used to scale its captured rectangle to the meter image size. The
LS-8000 record's rectangle (lines 56 to 501, pixels 91 to 1402 at 666 by 333
dpi) is upstream's 645 crop. The LS-5000 record's rectangle is upstream's 135
negative crop.

Tests (`src/scan/meter/mod.rs`):

- `an_ls5000_builder_record_is_exposed_as_nikon_exposes_it`: upstream's seed
  gives Nikon's first meter exposures, and its step gives Nikon's final
  exposures.
- `the_ls8000_analyzer_matches_the_builder_record`: the 645 crop is Nikon's
  rectangle, and all 12 analyzer limits match.
- `an_ls8000_builder_record_is_exposed_as_nikon_exposes_it`: Nikon's three
  final exposures; the LS-9000 curve does not give them.
- `a_low_first_ls8000_pass_is_metered_again`: an underexposed first pass is
  metered again before the analyzer.
- `the_nikon_negative_meter_is_verified_per_unit`.

## LS-8000 own film base exposure

Files: `src/scan/meter/ls8000.rs`, `src/scan/meter/mod.rs`,
`src/scan/autoexpose.rs`, `src/session/autoexpose.rs`, `src/bin/nkscan/cli.rs`,
`src/bin/nkscan/scan.rs`, `src/bin/nkscan/io.rs`, `src/python.rs`, and
`nkscan.pyi`.

Reason (2026-10-31): the raw LS-8000 negative showed a green film base on every
frame. LS8000.md3 sets all three exposures from one shared black point. That
point is the lowest channel's curve height at its thinnest point
(`ls8000::final_exposures`, `bottom`). The other channels are exposed to that
height, so they get less light than their own film base needs. On the Vision3
500T roll (frame 000) green sets the point. Red is exposed 0.11 density and blue
0.25 density short of their own thinnest points, and the raw film base read red
0.73 and blue 0.47 of green. The LS-50/LS-5000 rule exposes each channel to its
own thinnest point, so its film base reads near neutral.

Change:

- `ls8000::final_exposures` returns `ls8000::Final`: `nikon`, Nikon Scan's
  three exposures as before, and `own_base`. In `own_base` each channel is
  exposed to its own thinnest point, `base × 10^high_density`. The channel that
  sets the black point keeps Nikon's exposure. When Nikon leaves that channel
  headroom below its thinnest point (a narrow range moves the black point
  down), every channel keeps the same headroom. No channel gets less light
  than Nikon gives it.
- `Metering::own_film_base` (default `true`) selects `own_base` for the full
  scan. `Metering::step` then answers `Step::DoneOwnBase { scan, nikon }`. Both
  lists go through the unit's exposure limit. A locked white balance, other
  scanners and other film types are unchanged and answer `Step::Done`.
- `MeteringEvidence::nikon_exposures` keeps Nikon's exposures of every host
  metering. The meter record writes them as `nikon_final=`, and Python as
  `ScanResult.meter_nikon_exposures`.
- `scan --nikon-exposures` sets `own_film_base` to `false`.

nkcolor packages `nikon_final` as the capture manifest's `scan.exposures` and
the scanned values as `scan.scanned_exposures`. Its Nikon reconstruction
rescales each channel by Nikon's exposure over the scanned exposure before it
reads the pixels (`nkcolor.capture.to_nikon_exposures`). The Nikon output
therefore does not change. The stored raw scan and the raw view keep the
balanced film base.

Tests:

- `meter::tests::an_ls8000_negative_is_exposed_to_each_channels_own_film_base`:
  on the 2026-09-10 builder record, Nikon's exposures stay 879020, 3701152,
  4671874. The scan uses 879020, 3754018, 11429883: red sets the black point
  and keeps Nikon's value; green and blue are larger by
  10^(thinnest − Nikon's). `own_film_base = false` and a locked white balance
  give Nikon's exposures.
- `meter::tests::an_ls8000_builder_record_is_exposed_as_nikon_exposes_it` now
  checks Nikon's exposures through `Step::nikon`.
- `io::tests::a_meter_record_keeps_nikons_exposures_beside_the_scanned_ones` and
  the no-line case in `a_meter_record_keeps_each_pass_and_its_measured_rectangle`.

Hardware still to check on Windows: the LS-8000 accepts the longer blue
exposure (2.45 times Nikon's on the builder record) and the scan time it
gives; the raw film base reads near neutral.

## Meter evidence

Files: `src/scan/autoexpose.rs`, `src/session/autoexpose.rs`,
`src/session/data.rs`, `src/bin/nkscan/io.rs`, and `src/python.rs`.

`Session::autoexpose_frame_evidence_with` and `Session::autoexpose_with_evidence`
run upstream's metering loop and return `Metered`: the chosen exposures and
`MeteringEvidence`. Each `MeterPass` has the decoded pixels, the exact windows,
the pass layout, the exposures used for that pass, and `analyzer_rect`.
Upstream's `autoexpose_frame_with` and `autoexpose_with` call them and discard
the evidence. `frame::Scanned` gains `windows` (the full pass) and `metering`.

`Session::setup_record` returns the exact bytes of one per-color 8Dh setup
record. `Session::setup` parses those bytes through the same method.

Test: `io::tests::a_meter_record_keeps_each_pass_and_its_measured_rectangle`.

## Frame reuse, manual offsets, overscan, and focus

Files: `src/scan/frame.rs`, `src/scan/framing.rs`, `src/scan/focus.rs`, and
`src/scan/thumbnail.rs`.

Upstream owns frame discovery and placement. In 0.13 it ported Nikon Scan's
frame search (`scan::boundary`). It scans the rectangle it is given, registers
a moved rectangle in a perforation-framed unit's table, and puts the measured
table back after every pass.

The fork adds beside it:

- `frame::Options` fields `refocus_before_scan`, `overscan`,
  `nikon_negative_meter` and `clean_quality`; `Scanned::windows` and
  `Scanned::metering`.
- `framing::overscan_rect`: expands the full pass on a window-addressed unit,
  inside the address range; refused on a perforation-framed unit, whose 135
  frame is already the whole gate. Tests cover centering, range clamping, a
  full-width LS-8000 frame, and the refusal.
- An incomplete pass is `Error::IncompleteScan`; nothing is cleaned or
  published.
- `focus::focus_in_table` (LS-8000 strip holders, `Framing::Thumbnail`): a
  focus point in no frame-table entry is refused by the unit at once, so the
  moved frame goes into the table for the autofocus and the table is put back.
  Upstream's `framing::register` still acts only on perforation-framed units.
  Tests: `focus::tests` (three table cases) and the ignored hardware test
  `a_moved_frame_is_focused_rather_than_refused`.
- `thumbnail::strip_height`: inside upstream's `thumbnail::area`, the strip
  adapter branch only. Tests: `an_ls50_empty_y_range_uses_the_full_sa21_transport`,
  `an_ls50_empty_y_range_uses_the_full_sa30_transport`,
  `a_strip_thumbnail_uses_the_advertised_y_maximum`.

Placement must be re-accepted on LS-50 and LS-8000 hardware under the upstream
rule. The old accepted values are in `docs/LS50_POSITIONING_BASELINE.md`.

## Scanner calibration

Files: `src/session/mod.rs`, `src/protocol/cdbs.rs`, and `src/protocol/data.rs`.

`Session::calibrate_shading` runs the advertised `SetupShading` operation (B0h),
Nikon Scan's Calibrate, only when asked. It returns `false` when the scanner
does not offer the operation. The LS-50 and LS-8000 both advertise it; Nikon
gives about two minutes on the LS-8000 and LS-9000, inside `MOVE_TIMEOUT`
(180 s). Protocol tests check operation code B0h, the nine-byte SET PARAMETER
block, and the EXECUTE command. Evidence: three captured Nikon Scan sessions
never send B0h around previews or scans.

## Python API

Files: `src/python.rs`, `nkscan.pyi` (generated by
`cargo run --features python --bin stub_gen`), and `PYTHON.md`.

The fork adds these values:

- `Capabilities.x_boundary`, `Capabilities.y_boundary` and
  `Capabilities.exposure_range`.
- `Capabilities.calibrate` and `Session.calibrate_shading()` (stages the film
  again afterwards).
- `Discovery.thumbnail_bounds`: the scanner-address rectangle of the thumbnail
  window.
- `Discovery.thumbnail_frames`: on perforation-framed units, each frame's first
  and last thumbnail column and its feed extent. It is now read off upstream's
  `Discovery.column_addresses`: each end is the column whose address is
  nearest. Upstream's columns follow the perforation readings, so they are not
  evenly spaced. The fork used to compute the columns from a line pitch that
  upstream no longer publishes on these units.
- `MeterPass` with color pixels, optional infrared pixels, size, resolution,
  exposures and `analyzer_rect` (new in this sync).
- `ScanResult.meter_startup_exposures` and `ScanResult.meter_passes`.
- `ScanResult.meter_nikon_exposures` (2026-10-31): Nikon Scan's exposures
  for the frame. See [LS-8000 own film base exposure](#ls-8000-own-film-base-exposure).
- `Session.scan_frame(..., nikon_negative_meter=..., overscan=...,
  clean_quality=...)`. `positive` and `frame_offset` are accepted and ignored
  so older callers keep working.
- `clean_rgb_ir(rgb, infrared, model, dpi, quality)` runs the same saved-scan
  dust processor as `nkscan clean`. It returns cleaned RGB pixels and the
  rebuilt-pixel count.

`Session.focus_frame` is upstream's. It also accepts `at=(x, y)`, the fork's
earlier call, and then reports "not-reached" with a hyphen as that call did.

The extension starts logging when the module loads. It calls upstream's
`init_logging("nkscan=debug")` on stderr, so `RUST_LOG` still overrides the
level and a later `init_logging` call changes the level and the destination.
The fork used to install its own subscriber at import, which would have made
upstream's `init_logging` do nothing. The Windows nkcolor launcher captures
stderr in its stable server log.

Upstream 0.13 to 0.17 added Python values the fork does not change:
`discover_frames(film=)`, `scan_frame(film=, format=, base_density=)`,
`meter_frame(film=, format=, base_density=)`, `Discovery.base_density`,
`Discovery.column_addresses`, `Discovery.row_addresses`,
`Capabilities.film_format` and `Capabilities.film_formats`.
`lock_white_balance` now defaults to the value for the film instead of `True`.

## Changes callers must follow

nkcolor and nkcolor-webui keep working without a change. These points change
behavior or offer something better:

- `discover_frames` without `film=` takes the thumbnail at upstream's positive
  exposure. Pass `film="negative"` for Nikon Scan's negative thumbnail exposure
  and a measured `Discovery.base_density`. The webui call is in
  `nkcolor-webui/src/nkcolor_webui/scanner.py` (`DirectNkscan`,
  `discover_frames(format=..., progress=...)`).
- `scan_frame` without `film=` meters as a positive unless
  `nikon_negative_meter=True`. The webui's `DirectNkscan.preview()` passes no
  film. It has no production caller; the live preview uses the command line.
- The meter crop is upstream's, not the scaled reference rectangle nkcolor
  writes as `nikon_analyzer_rect` (`nkcolor/src/nkcolor/capture.py`, from
  `ScannerPipeline.analyzer_rectangle` in `nkcolor/nikon/profiles.py`). The two
  agree on the two builder records. They can differ when the meter image is not
  413 by 281 (LS-50/LS-5000) or not a 645 frame at 1494 by 558 (LS-8000).
  nkcolor should read the new `analyzer_rect=` line of the last pass instead.
- `Discovery.column_addresses` and `row_addresses` (upstream) place any frame
  on the thumbnail, on every framing mechanism and on both strips of an
  FH-835S. `thumbnail_frames` stays perforation-only.
- `--nikon-meter` on an LS-8000 now uses the LS-8000 curve through upstream's
  metering, and plain LS-8000 negative metering does too.
- An unrecognized scanner refuses `--clean` (unchanged from the fork; upstream
  cleans it with the LS-9000 profile).
- An LS-8000 color negative is scanned at its own film base exposures. A
  caller that rebuilds Nikon's image must read Nikon's exposures from
  `nikon_final=` or `ScanResult.meter_nikon_exposures` and rescale the pixels
  (nkcolor does this in `nkcolor.capture.to_nikon_exposures`).

## Upstream reconciliation

### 2026-10-27: sync to 0.17.0

Upstream moved from `942121c` (0.12.0) to `v0.17.0` (`0857681`), 50 commits.
The fork's 30 commits were rebased onto `v0.17.0`, one commit per fork change,
with the same subjects. One commit was dropped; three were reduced to what
upstream does not cover. The old branch is `main-pre-0.17`.

| Fork commit | Status | Reason |
| --- | --- | --- |
| build the capture proxy for USB or FireWire and log the builder | Kept | Upstream raised the overlapped READ wait to 600 s. The fork keeps that wait and marks a READ that is still pending as not captured. |
| treat the FH-835M and FH-835S holders as 135 film | Kept | Upstream still offers 120 choices. With 135 resolved, upstream's metering uses its 135 crop on these holders; no Nikon capture checks that crop on an LS-8000/LS-9000. |
| log why a scanner path does not open or answer INQUIRY | Kept | Ported to the async probe loop. |
| log every SET WINDOW record | Kept | |
| log which focus mode a pass uses | Kept | |
| return the raw 8Dh setup bytes from Session::setup_record | Kept | Now async. |
| read every thumbnail as positive film | **Dropped** | Upstream 0.13 sets `POSITIVE \| AVERAGING` on every thumbnail window, as Nikon Scan does; its window tests match Nikon's LS-50, LS-4000 and LS-9000 window bytes. The fork's LS-8000/LS-9000 window also left out averaging, which Nikon Scan sets. |
| give a strip adapter with a zero Y maximum the transport span | Reduced | Upstream's `thumbnail::area` now chooses the thumbnail window. The fork's span applies only in its strip-adapter branch. |
| clean dust with a verified profile per model and a quality | Kept | |
| make --clean and --ir exclusive | Kept | |
| clean a saved scan and its infrared file without a scanner | Kept | |
| ignore the reserved AE bits the LS-8000 reports | Kept | Upstream still trusts the bits. |
| add Nikon Scan's color-negative meter to the library | **Reduced** | Upstream's port replaces the fork's LS-50/LS-5000 analyzer and ladder (same result on the LS-5000 record). Only the LS-8000 curve and analyzer (`meter::ls8000`) remain. |
| keep meter evidence and offer the Nikon negative meter | Reduced | Evidence ported onto upstream's loop. `--nikon-meter` now selects upstream's negative metering and the unit check; the meter record gains `analyzer_rect`. |
| make an incomplete pass an error | Kept | |
| overscan the full pass on window-addressed units | Kept | |
| autofocus an LS-8000 frame moved past its table entry | Kept | Upstream's `framing::register` covers perforation-framed units only. |
| scan at known exposures with --exposure R G B | Kept | |
| choose the focus point, lens position and refocus on the CLI | Kept | |
| reuse saved frame rectangles and move frames on the CLI | Kept | A reused rectangle is metered with the default base density. |
| print machine-readable progress with NKSCAN_PROGRESS_LINES | Kept | |
| capture a normal and Super Fine pair with --superfine-pair | Kept | |
| log the submitted and scanned rectangle of each pass | Kept | |
| add Nikon Scan's Calibrate as an on-demand command | Kept | Upstream defines `SetupShading` and still never sends it. |
| add Python exposure range, boundaries and thumbnail frames | Reduced | `thumbnail_frames` is read off upstream's `column_addresses`. |
| install the tracing output when the Python module loads | Reduced | Now a call to upstream's `init_logging`. |
| keep the fork's older Python calls working | Kept | |
| document the fork changes | Kept | This file, CHANGELOG, PYTHON.md. |
| describe the fork in the README | Kept | |
| log the scanner lifecycle | Kept | Also logs upstream's new `Session::close`. |

Verification on 2026-10-27, macOS, no scanner:

- `cargo build -j 2` and `cargo test -j 2`: 207 library tests pass; 3 hardware
  tests ignored; 1 doctest passes.
- `cargo test -j 2 --all-features`: also 20 command-line tests pass.
- `cargo clippy --all-targets --all-features` and `cargo fmt --check`: clean.
- `cargo check --target x86_64-pc-windows-msvc --features cli`: clean.
- Each of the 29 commits passes `cargo check --all-features --tests` and
  `cargo fmt --check`.
- `maturin build --release --features python` builds the abi3 wheel. The
  wheel imports in Python 3.13, has every value above, and `clean_rgb_ir` runs.

Hardware still to check: the LS-8000 and LS-50 meter and frame placement under
0.17, and the FH-835S 135 crop.

### 2026-10-09: rebase onto 942121c

See [docs/UPSTREAM_AUDIT_2026-10-09.md](docs/UPSTREAM_AUDIT_2026-10-09.md).
What upstream then covered was dropped: the perforation-unit autofocus
registration, the frame-table restore, the multi-sample READ fixes, and the
fork's placement.

## Nikon Scan capture proxy

Files: `scsi_proxy/proxy.c`, `scsi_proxy/nkduscan.def`, and
`scsi_proxy/README.md`.

One proxy source can build as the USB `NKDUSCAN.dll` proxy or the FireWire
`NKDSBP2.dll` proxy. Build definitions select the real DLL name, the displayed
name, and the trace prefix. `NIKON_CAPTURE_DIR` selects the output directory.
Separate file names prevent one inactive transport from replacing the active
transport's trace.

The proxy also hooks the four direct calls to the LS-5000 negative table
builder at `LS5000.md3:0x10011840`. Each `nikon_builder_NNNN.bin` file contains:

1. the builder descriptor before the call;
2. the meter pixels read by the builder;
3. the 1,024-byte characteristic resource;
4. the function result;
5. the descriptor after the call; and
6. the generated table bytes.

The hook keeps the original builder function intact. It replaces only the four
direct call targets. This lets the hook call the original function without a
trampoline.

## Documentation and release notes

Files: `README.md`, `PYTHON.md`, and `CHANGELOG.md`.

These files describe the new command options, Python fields, scanner fixes,
and evidence files. The changelog keeps one "nkcolor fork" section above
upstream's releases. Update this document and that section when the fork
changes.

## Verification

The Python feature needs a Python 3.13 interpreter for PyO3. Set
`PYO3_PYTHON` when the default `python3` is older.

Run all Rust and Python-feature tests:

```sh
cargo test --all-features
```

Build the command used by nkcolor:

```sh
cargo build --release --features cli
```

Build the Python wheel, as `nkcolor-webui/tools/build_standalone.py` does:

```sh
python -m maturin build --release --features python
```

Regenerate the Python stub after a Python API change:

```sh
cargo run --features python --bin stub_gen
```

Before a release, compare `git diff upstream/main` with this file. Keep the
upstream remote at `https://github.com/activexray/nkscan.git`.
