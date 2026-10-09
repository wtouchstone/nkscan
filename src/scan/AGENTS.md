# Scanner scan-component instructions

Read the workspace RULES.md before each task.

## Frame placement follows upstream

On 2026-10-09 the user reopened the LS-50 and LS-5000 positioning work and
chose upstream nkscan's placement (0.11 and later): `scan_frame` scans the
rectangle it is given, with no meter-pass re-placement. Do not reintroduce the
fork's old re-placement, output-column selection, or 5670 frame extent. Keep
placement code identical to upstream; put nkcolor additions beside it.

The old accepted values are kept in docs/LS50_POSITIONING_BASELINE.md for
comparison. New LS-50 and LS-8000 hardware runs must re-accept placement
before real scanning.

After a change here, run:

    cargo test scan::
    cargo test --test hardware --no-run

Also run nkcolor-webui/tests/test_scanner.py from the Web UI environment.
