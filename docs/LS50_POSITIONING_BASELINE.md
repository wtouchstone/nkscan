# LS-50 and LS-5000 positioning baseline

**Superseded 2026-10-09.** The user reopened the positioning work and chose
upstream nkscan's placement: the rectangle given is scanned as given, with no
meter-pass re-placement, 135 frames are 5959 addresses, and rectangles are one
past the last line and column. The values below are the old fork's accepted
result, kept for comparison. LS-50 and LS-8000 hardware runs must re-accept
placement under the upstream rule.

The accepted LS-50 hardware result from 2026-09-11 is:

```text
detected=17921:0:23590:3945
requested_feed_offset=0
meter_offset=336
correction=191
final=18112:0:23781:3945
pass=18112:0:24071:3945
output_columns=127..5785
```

The protected path includes:

- Frame detection and the measured thumbnail line pitch in
  `src/scan/thumbnail.rs`.
- Perforation-table mapping, registration, and meter correction in
  `src/scan/framing.rs`.
- Full-gate pass placement and output-column selection in
  `src/scan/frame.rs`.

The empty-range thumbnail startup repair is separate. It only selects the
length of the initial strip thumbnail pass. It must not change any operation
listed above.
