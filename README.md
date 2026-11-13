# nkscan (nkcolor fork)

What this fork adds to upstream nkscan
([activexray/nkscan](https://github.com/activexray/nkscan)).

## Metering and exposure

- Meters exactly like Nikon Scan on the LS-50, LS-5000 and LS-8000. The
  exposures match Nikon Scan's.
- Meters black and white negatives the way Nikon Scan does (LS-50 and
  LS-5000).
- The LS-8000 uses its own film curve for metering. Upstream uses the
  LS-9000 curve.
- The LS-8000 gives each colour enough light for its own film base. The raw
  film base comes out neutral. Nikon Scan's exposures are still saved.
- The LS-8000 is metered on the computer, as Nikon Scan does. The scanner's
  own automatic exposure is ignored.
- Saves the metering scans, the exposures and every scanner setting. This
  lets Nikon's colour be rebuilt later.
- Scans at exposures you give. This repeats an earlier scan exactly.
- A reused frame is metered with the film base from its thumbnail.

## Framing and focus

- Reuses frame positions from an earlier thumbnail pass. No second
  thumbnail pass.
- Moves a frame along or across the film.
- Overscan: scans a margin around the frame.
- Scans the whole sensor width, for 120 film with its border.
- Focuses on a point you choose, at a fixed lens position, or again just
  before the scan.
- The LS-8000 can autofocus on a frame you moved.
- The LS-50 strip adapter gets its full thumbnail length when it reports
  zero.
- The FH-835M and FH-835S holders are treated as 35 mm.

## Scanner

- Calibrate: Nikon Scan's Calibrate button. The scanner measures its light
  again.
- Super Fine pair: scans one frame in normal mode and in Super Fine mode.
- In very dense film, each sensor line keeps its correction in the darkest
  values too. This has no effect on the LS-8000.
- Writes a small file that says which sensor line made each pixel row.

## Dust removal

- Uses the right dust profile for each scanner model, the LS-8000 too.
- Offers Nikon's Normal and Fine settings.
- Refuses a scanner it has no profile for.
- Cleans a saved scan later, without the scanner.
- Cleaning and keeping the infrared layer cannot be used together.

## Safety and logs

- A scan that stops early is an error. No broken file is written.
- Prints progress lines that the web app reads.
- Logs when nkscan starts and stops, and when the scanner opens and closes.
- On Windows, logs slow and refused scanner commands.
- Logs each scanner setting, the frame position, the focus mode, and why a
  scanner did not open.

## Other

- Capture proxy: a small Windows file that records everything Nikon Scan
  sends to the scanner. We use it to check our results.
- More Python calls: exposure range, Calibrate, thumbnail frames, metering
  data, and cleaning a saved scan.
- This fork publishes nothing to crates.io or PyPI.
