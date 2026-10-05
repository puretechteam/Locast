# Playwright baseline

CI runs the Playwright suite as two jobs (`.github/workflows/ci.yml`):

- `e2e gate`: hard gate. Runs the spec files listed in the `test:e2e:gate`
  script in `apps/client/package.json`. Any failure fails CI.
- `e2e full-suite baseline`: non-gating. Runs every spec, lists every failing
  test in the run summary and as warning annotations, and uploads the report,
  traces and videos.

When a failing spec below is fixed, add its file to `test:e2e:gate` (when the
whole file is green) and delete its row here. `laser_e2e`, `leave_room_modal_e2e`
and `permission_e2e` are now fully green and in the gate. `chat_e2e` (6 of 8
pass) and `host_disconnect_grace_e2e` are not, so they stay out of the gate.

Nothing is skipped, deleted or loosened. Each row below is a failing test as of
the baseline run (121 tests: 109 passed, 12 failed); 9 of the 12 have since
been fixed and their rows removed (3 remain). The classification comes
from reading the spec against the code; "unresolved" means the cause was not
established.

| Spec | Test | Class | Evidence |
| --- | --- | --- | --- |
| chat_e2e:275 | quality bar is present on each tile | stale expectation, plus a product bug | `ParticipantTile` renders `.participant-tile__quality` only when quality is not "good" (the default), and has since P6-T04; the spec expects it on every tile. Separately, the shim has no `clock_skew_probe` handler, so the probe fails and the store goes to "poor", but the tile never shows it: the `React.memo` comparator on `ParticipantTile` compares only `participant`, so a `quality` change never re-renders the tile. Fixing the spec alone cannot make it green. |
| chat_e2e:286 | quality bar shows poor class when probe responses are delayed | stale harness, plus the same product bug as above | The spec delays an HTTP route `**/v1/call/clock_skew_probe`; the app calls `clock_skew_probe` over Tauri IPC (the shim), so the route is never hit. Even the shim's failed probe (store goes to "poor") does not reach the DOM because of the `ParticipantTile` memo comparator. |
| host_disconnect_grace_e2e:66 | grace banner; RoomClosed returns to empty state | stale harness (medium confidence) | The spec seeds the summary with `emitCapabilityUpdate`; `RoomTopBar` only sets `prevSummaryRef` from `roomState` events, so the "Room ended" toast never arms. |

No failure was attributed to the browser or the CI environment.

## Infrastructure failures fixed with the CI work

`chat_e2e` (8 tests) and `leave_room_modal_e2e` (2 tests) called
`page.waitFunction`, which is not a Playwright API (`waitForFunction`). Fixing
the name turned 5 of the chat failures green. `tests/playwright` is not covered
by `pnpm typecheck`; running `tsc -p tests/playwright/tsconfig.json` reports
about 30 further errors, so that project is not yet a gate.
