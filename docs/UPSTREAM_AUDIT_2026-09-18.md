# Upstream nkscan audit (2026-09-18)

This audit compares local `main` at `fde82a5` (0.11.0) with upstream
`f847874` (0.12.0). The upstream remote is
`https://github.com/activexray/nkscan.git`.

The fork keeps Nikon evidence capture, Nikon negative metering, frame reuse,
manual offsets, shading records, progress records, and the Python acquisition
API. An upstream change is adopted only when it preserves those interfaces and
the locked LS-50/LS-5000 positioning path. “Manual” means a focused review and
hardware or capture evidence is required before a source change. “Retain” means
the fork already has an equivalent or deliberately different implementation.
“Remove” means the upstream code is not part of this fork.

| Upstream commit | Decision | Exact decision |
| --- | --- | --- |
| `9f855bc` | Retain local | The fork already installs a Python tracing subscriber at module import with `nkscan=debug,nusb=warn`. Upstream's explicit `init_logging()` would do nothing after that subscriber exists. Keep the automatic logging needed by the Web UI. Add a separate level control only if the caller needs one. |
| `f14ec75` | Retain local | Upstream removes the Nikon meter rectangle, frame columns, polarity, measured gate offset, and recentering. The fork needs these interfaces for Nikon evidence and the locked LS-50/LS-5000 placement path. Keep the fork's caller-selected rectangle behavior. |
| `16de9bf` | Required manual port | The fork restores the measured frame table only after a successful final pass. Focus, metering, window construction, overscan rejection, cancellation, and scan errors can leave a temporary table installed. Move restoration around the complete operation while retaining the fork's meter correction, user offset, and evidence paths. |
| `48e7122` | Split/manual | Keep the local detector, Nikon meter, evidence examples, and locked placement interfaces. Remove only code proven dead in the fork. Port the LS-5000 multi-sample plus infrared per-reading truncation groundwork (`infrared_reading`, `truncated_bytes_once`, and per-reading stripping) before the later READ granule fix. |
| `a64db4c` | Manual/deferred | Keep 5670 as the detected 135 picture extent because the fork separately scans the full 5959 gate and its LS-50 baseline depends on that split. The fork calculates extents as `bottom - top` and uses Rust half-open ranges, while its wire encoding and decoding still treat the last coordinate as inclusive. Migrate that coordinate boundary only with all Python and Web UI callers and new hardware evidence. A direct port changes the accepted LS-50 detected rectangle by one address unit. |
| `e174a0b` | Documentation only | This commit changes only changelog wording. The half-open rectangle implementation is in `a64db4c`; review and port it there. |
| `7201ef1` | Do not port | Upstream's fixed `GATE_OFFSET = 100` was temporary and `5c12bab` later removed it. Keep the fork's measured `gate_offset` and full-gate path. |
| `1529980` | Adopt | Remove `review.rs`. The CLI no longer includes it and no workspace caller uses it. |
| `e769622` | Adopt | Remove the unused `Channel::visible_index` helper in a source cleanup. No caller in the fork reads it. This is safe after a compile check. |
| `ba14399` | Retain local | The fork already publishes direct `thumbnail_frames` values, including its locked perforation-placement data. Do not add a second mapping interface. |
| `6985021` | Skip | This changes only upstream changelog wording for behavior the fork does not use. |
| `e63cca2` | Adopt | Update the `Session::frames` and `frames_type2` documentation to state the table-specific meaning and why one accessor cannot answer for the other table. |
| `ced59e3` | Skip | This changes only upstream changelog wording. |
| `5c12bab` | Retain local | This removes the temporary fixed gate offset from `7201ef1`. The fork already uses the measured gate offset and full-gate pass. |
| `e9cdffb` | Skip | This changelog cleanup describes upstream removals that do not apply to the fork. |
| `f5564e3` | Required manual port | The fork lacks the READ granule calculation and can end an LS-5000 multi-sample plus infrared READ inside one reading when a line exceeds the transfer size. Port it after the per-reading truncation work from `48e7122`, then run the protocol and byte-for-byte tests. |
| `982ba6f` | Port with `f5564e3` | The current toolchain supports `is_multiple_of`. Bring this test cleanup with the READ granule port. |
| `4f7ac64` | Split/manual | Port the partial-pass view and gap-density scoring for thumbnail discovery. Keep the fork's stronger final-frame rule: an incomplete full scan fails and no image is published. Validate both parts with captured short reads before hardware use. |
| `f847874` | Defer | Keep the fork version unchanged until the selected upstream work is integrated and verified. |

## Test status

The audit itself did not import upstream source. Separate fork work added the
overscan path during the same work session. `cargo test --no-default-features`
passes: 178 unit tests, one ignored hardware test, and one doctest.

`cargo test --all-features` could not start because the available interpreter
is Python 3.8 while the `abi3-py313` feature requires Python 3.13 or newer.
No hardware scan was run. Resolve the frame-table restore before the next
perforation-scanner test. Resolve the READ and padding work before the next
LS-5000 multi-sample plus infrared test. The other Split/manual entries remain
deferred until their affected positioning contract is reopened.
