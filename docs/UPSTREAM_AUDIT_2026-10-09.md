# Upstream nkscan audit (2026-10-09)

Local fork before the rebase: every nkcolor change on top of upstream 0.11.0
commit `fde82a5` (snapshot `54a5191`, since deleted at the user's request).
Upstream: `942121c` (after 0.12.0), 27 commits after the fork's base `34fde66`.
Upstream dropped `fde82a5` and moved the v0.11.0 tag to `e9cdffb`; `fde82a5`
changes only the changelog and version files.

A trial rebase (`git rebase --onto upstream/main fde82a5`) conflicts in 15
files, about 70 blocks: `src/python.rs` 17, `src/scan/framing.rs` 8,
`src/scan/meter.rs` 7, `src/scan/frame.rs` 6, `src/session/autoexpose.rs` 6,
`nkscan.pyi` 5, `src/scan/thumbnail.rs` 4, and the rest 1 or 2 each. Upstream
deleted `src/scan/boundaries.rs`, which the fork's placement code uses. The
rebase was aborted; nothing was changed.

Status words: **Keep** = upstream has no equivalent. **Replace** = upstream now
does the same; drop the fork code. **Split** = upstream covers part.
**Decision** = protected path (`docs/LS50_POSITIONING_BASELINE.md`); the user
must reopen it.

## Fork changes against upstream 0.12

| Fork change (NKCOLOR.md) | Status | Evidence |
| --- | --- | --- |
| Meter evidence: `scan --meter`, meter-v2 files, SET WINDOW and 8Dh setup records, `Session::setup_record` | Keep | Upstream has no meter evidence output. nkcolor's reconstruction needs it. |
| Nikon negative meter: `--nikon-meter`, LS-50/LS-5000 and LS-8000 analyzers, four-pass sequence | Keep | Upstream 0.12 `frame::meter_frame_with` / Python `meter_frame` is the generic meter. |
| Known exposures: CLI `--exposure R G B` | Keep (CLI) | Upstream takes exposures back into `scan_frame` in Python only; its CLI has no option. |
| Focus: CLI `--focus X Y`, `--focus-position`, `--refocus-before-scan` | Split | Upstream 0.12 `frame::Options::focus`, Python `scan_frame(focus=...)`, `focus_to`, `focus_position`. Its CLI has none of these. Use upstream's Python API; keep CLI flags only if nkcolor calls the CLI with them. |
| AF fix: `focus_on_frame` registers the frame, focuses, restores the table (2026-10-05) | Split | Upstream `frame::focus_frame` uses `at_frame`: `framing::register` then restore. Same fix on perforation-framed units. Upstream `register` returns at once unless `Framing::Perforation` (`framing.rs:193`), so the fork's LS-8000 case (focus point in no rectangle) is still needed. |
| Python tracing subscriber at import | Replace or keep | Upstream `init_logging(level)` (`9f855bc`). The Web UI wants logging on without a call; calling `init_logging` at import would do it. |
| Frame table restored only after a corrected pass | Replace | Upstream `16de9bf` restores after any pass, finished or not. The 2026-09-18 audit already marked this a required port. |
| Multi-sample with infrared, READ length | Replace (adopt) | Upstream `48e7122` per-reading truncation, `f5564e3` READ ends on a reading. The fork lacks both; LS-5000 multi-sample plus IR needs them. |
| Reject incomplete frame output (exit failure) | Split | Upstream 0.12 reports `ScanResult.complete` and `blocks` and keeps the file. The fork fails the scan. Either keep the fork rule or adopt upstream's flag and have nkcolor refuse to publish an incomplete pass. |
| Shading calibration before every preview and scan (not LS-8000) | Changed | Upstream defines `SetupShading` (`protocol/data.rs:745`) but never issues it. Nikon Scan never sends it before a scan either, so it is now an on-demand Calibrate command on every unit that advertises it. |
| LS-8000: raw (positive) thumbnail, full thumbnail height, dust constants, ignore reserved AE bits | Keep | Upstream has no LS-8000-specific handling of these. |
| `--clean` and `--ir` exclusive; `clean.rs`; Python `clean_rgb_ir` | Keep | Upstream has no saved-scan cleaner in Python. |
| `NKSCAN_PROGRESS_LINES` machine progress | Keep | Upstream has none. |
| Python `Capabilities.x_boundary/y_boundary/exposure_range`, `MeterPass`, `ScanResult.meter_*` | Keep | Upstream has none of these fields. |
| Python fields upstream now has (`focus_position`, focus result) | Replace | Use upstream's names (`ScanResult.focused`, `focus_position`). |
| Capture proxy (`scsi_proxy/`): USB/FireWire build, builder hook | Keep | Research tooling; upstream has none. |

## Protected placement path (decision)

| Fork behaviour | Upstream 0.11/0.12 |
| --- | --- |
| The meter pass re-places a reused rectangle ("meter correction", `boundaries::locate`, `recentered_with_adjustment`). | `f14ec75`: `scan_frame` scans the rectangle it is given. `recentered`, `pass_rect`, `boundaries`, `Polarity`, `gate_offset`, `frame_lines` are gone. |
| Large user offsets: the 2026-10-02 fix stops the meter re-placing onto the neighbour's picture. | Fixed by removing re-placement ("A rectangle handed to `scan_frame` ... is scanned where it says"). |
| Detected 135 extent 5670, full 5959 gate pass, output-column selection (`frame_columns`). | `a64db4c`: 135 is 5959 throughout; the pass is the frame; no column selection. |
| Inclusive rectangles on the wire. | `e174a0b`: `Rect` is one past the last line and column; converted at the record. |
| Frame detection and line pitch as accepted on 2026-09-11. | `6d91485`, `ba14399`, `4f7ac64`, `5311b44`: fit by bare film, one measurement for both tables, short-pass framing, gap-versus-sky test, film end from padding. |

Option A, keep the fork path: restore `boundaries.rs` and the removed
functions inside the fork, resolve the placement conflicts toward the fork, and
take upstream everywhere else. Placement does not change. The fork stays far
from upstream, so every later rebase repeats this work.

Option B, adopt upstream placement: drop the fork's re-placement, column
selection, 5670 extent and large-offset fix; a user offset becomes a moved
rectangle that upstream scans as given. The Web UI scan code that reads
`frame_columns` and passes `positive` must change. The accepted LS-50 numbers
from 2026-09-11 change (5959 frame, half-open rectangle), so LS-50 and LS-8000
hardware runs must re-accept placement before real scanning.

## Outcome (2026-10-09)

The user chose Option B. Branch `nkcolor-0.12` starts at upstream `942121c`
and ports the fork one area per commit: fork-only files; protocol and session
evidence; the Nikon meter; frame options and evidence; thumbnail windows; the
CLI; the Python API; docs. `cargo test --all-features` passes (175 library, 19
CLI, stub generator; 3 hardware tests ignored without a scanner). Dropped:
`boundaries`, re-placement, `recentered_with_adjustment`, `feed_offset`,
output-column selection, `remember`, `focus_on_frame` (perforation half),
`frame_columns`. Kept for callers: `scan_frame(positive=, frame_offset=)`
accepted and ignored; `focus_frame(at=)`. Hardware runs on the LS-50 and the
LS-8000 must re-accept placement before real scanning.
