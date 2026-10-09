# nkscan (nkcolor fork)

This is nkscan with additional changes for exact Nikon conversion and some QOL
improvements.

Upstream is [activexray/nkscan](https://github.com/activexray/nkscan). Its
documentation covers everything not listed here. NKCOLOR.md has the technical
detail for each change.

## Changes for exact Nikon conversion

**Nikon's negative metering** (`scan --nikon-meter`, Python
`nikon_negative_meter=True`)
Upstream meters the way Nikon Scan does. This option meters as a color negative
and refuses a scanner that no Nikon Scan capture checks: it runs only on the
LS-50, LS-5000 and LS-8000. On those three the final exposure matches Nikon
Scan's to the count. Nikon's color conversion depends on the exposure it chose,
so we have to choose the same one to get the same result.

**LS-8000 metering curve**
The LS-8000 uses its own film curve and its own form of Nikon's analyzer.
Upstream uses the LS-9000 curve on every medium-format scanner. The fork adds
the LS-8000 form, so the LS-8000 gets Nikon Scan's exposures.

**Saved meter data** (`scan --meter`, Python `meter_passes`)
Keeps every meter image, the exposure of each pass, the exact scanner settings
sent for each pass, and the scanner's setup records. Our software rebuilds
Nikon's conversion from these files. Without them the result can only be an
estimate.

**Capture proxy** (`scsi_proxy/`)
A small DLL that sits between Nikon Scan and the scanner driver and records
every command, for USB and FireWire scanners. It also records the inputs and
output of Nikon's color table builder. This is how we check our results against
Nikon Scan itself.

## Quality of life

**Scan with known exposures** (`--exposure R G B`)
Repeats an earlier exposure without metering again, so a roll or a re-scan
comes out the same.

**Choose the focus** (`--focus X Y`, `--focus-position`, `--refocus-before-scan`)
Autofocus on a chosen point of the frame, drive the lens to a known position,
or focus again just before the scan.

**Reuse frame positions** (`--frame-rect-for`)
Scans frames that an earlier thumbnail already found, without another thumbnail
pass.

**Move a frame** (`--frame-offset`, `--frame-offset-for`)
Shifts a frame along or across the film when the detected position is off.

**Overscan** (`--overscan`)
Scans a margin around the frame on scanners that address the film directly,
such as the LS-8000.

**Super Fine pair** (`--superfine-pair`)
Scans the same frame in the normal mode and in Super Fine, for comparison.

**Better dust cleaning** (`--clean-quality normal|fine`)
Uses the right dust profile for each scanner model, including the LS-8000, and
offers Nikon's Fine setting.

**Clean a saved scan** (`nkscan clean`, Python `clean_rgb_ir`)
Runs dust cleaning later on a scan saved with its infrared plane, without the
scanner.

**Cleaning and infrared output are exclusive**
`--clean` and `--ir` cannot be used together. Cleaning uses the infrared plane
itself; to clean later, save the infrared plane instead.

**Short scans fail**
A scan that ends before the frame is complete stops with an error and writes
no file, rather than writing an image with missing rows.

**LS-8000 autofocus on a moved frame**
The LS-8000 refused to focus on a frame moved past its detected place. The
moved frame now goes into the scanner's frame table for the autofocus.

**LS-8000 metering**
The LS-8000 reports automatic exposure features it does not really have. The
computer now meters, as Nikon Scan does on this model.

**LS-50 strip adapter span**
Some strip adapters report a length of zero before the first thumbnail. The
thumbnail now uses the length the scanner documentation gives.

**FH-835M and FH-835S holders**
These are 35 mm holders, so the film format is set automatically.

**Calibrate** (`nkscan calibrate`, Python `calibrate_shading()`)
Nikon Scan's Calibrate button. The scanner measures its light source again,
which fixes streaks along the direction the film travels. It runs only when
asked, as in Nikon Scan, and can take about two minutes on the LS-8000.
`Capabilities.calibrate` says whether a scanner offers it.

**Progress and logs for the web app**
`NKSCAN_PROGRESS_LINES=1` prints progress the app can read. Extra log lines
record each frame's position, each scanner setting, the focus mode, and why a
scanner failed to open.

**Python additions**
`Capabilities.exposure_range` and `calibrate`, `Discovery.thumbnail_bounds` and
`thumbnail_frames`, `ScanResult.meter_passes`, and `clean_rgb_ir`. Older
calls from our web app (`scan_frame(positive=, frame_offset=)`,
`focus_frame(at=)`) still work.
