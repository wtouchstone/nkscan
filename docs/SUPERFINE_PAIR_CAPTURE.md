# Matched LS-8000 normal and Super Fine capture

## Request

Capture one LS-8000 frame in normal three-line mode and Super Fine single-line
mode. Keep registration as stable as the scanner permits.

## Interface

`nkscan scan --superfine-pair` takes the normal scan first. It writes that TIFF
with the normal basename. It then writes the single-line TIFF as
`<basename>_superfine_<n>.tiff`.

The option cannot be combined with `--superfine`, `--ir`, or `--clean`. It
requires a resolution where the scanner supports both CCD modes.

## Acquisition sequence

The command discovers the frame once. It focuses once and meters once. It
keeps one scanner session open and reserved. The second pass copies the first
pass's complete SET WINDOW descriptors. It changes only byte 44 from
`MULTILINE_SIMULTANEOUS` to `LINE_WITHOUT_DISTANCE`.

The second full scan still moves the holder through the gate. The scanner
cannot produce a full second image while the carriage stays fixed. Pixel
registration remains necessary before a difference measurement.

## Files and component boundaries

- `src/bin/nkscan/cli.rs` defines and validates the new command-line option.
- `src/bin/nkscan/scan.rs` owns the paired acquisition sequence. Scanner
  protocol and frame-positioning code remain unchanged.
- `tools/nikon_capture/capture-ls8000-superfine-pair.ps1` fixes the research
  capture at LS-8000 frame 2, 645 format, and 4000 dpi. It keeps both TIFFs,
  meter evidence, the log, and checksums.
- `tools/nikon_capture/capture-ls8000-superfine-pair.cmd` is the Windows
  double-click entry point.

## Verification

- The CLI parser test rejects incompatible raw-input options.
- The scan test proves that the prepared descriptors differ only in their CCD
  interleaving field.
- `cargo test --features cli --bin nkscan` covers both tests.
- A Windows cross-build checks the test executable. A hardware run on Windows
  is required to verify scanner movement and the two saved images.
