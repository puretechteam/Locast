# Playwright baseline

CI runs the Playwright suite as two jobs (`.github/workflows/ci.yml`):

- `e2e gate`: hard gate. Runs the spec files listed in the `test:e2e:gate`
  script in `apps/client/package.json`. Any failure fails CI.
- `e2e full-suite baseline`: non-gating. Runs every spec, lists every failing
  test in the run summary and as warning annotations, and uploads the report,
  traces and videos.

When a failing spec below is fixed, add its file to `test:e2e:gate` (when the
whole file is green) and delete its row here.

Nothing is skipped, deleted or loosened. Each row below is a failing test as of
the baseline run (121 tests: 109 passed, 12 failed). The classification comes
from reading the spec against the code; "unresolved" means the cause was not
established.

| Spec | Test | Class | Evidence |
| --- | --- | --- | --- |
| chat_e2e:264 | host participant has Host badge | stale selector | The badge is a child of `.participant-tile__name`; the spec takes its parent (`..`, which is that same name div) and looks for `.participant-tile__name` inside it. |
| chat_e2e:273 | quality bar is present on each tile | stale expectation or product decision | `ParticipantTile` renders `.participant-tile__quality` only when quality is not "good" (the default), and has since P6-T04. The spec expects it on every tile. |
| chat_e2e:284 | quality bar shows poor class when probe responses are delayed | stale harness | The spec delays an HTTP route `**/v1/call/clock_skew_probe`; the app calls `clock_skew_probe` over Tauri IPC (the shim), so the route is never hit. |
| host_disconnect_grace_e2e:66 | grace banner; RoomClosed returns to empty state | stale harness (medium confidence) | The spec seeds the summary with `emitCapabilityUpdate`; `RoomTopBar` only sets `prevSummaryRef` from `roomState` events, so the "Room ended" toast never arms. |
| laser_e2e:571, 614, 629, 646 | `d` key / Escape / pointer-events / `l` key while drawing | stale precondition | These specs press `d` without granting the DRAW capability. The toolbar is gated on `canDraw` (`DrawingLayer.tsx`), so it never appears. |
| leave_room_modal_e2e:36, 51 | Delete / Keep temp files | unresolved | These specs could never have passed: they called the non-existent `page.waitFunction`. With that fixed they time out waiting for `window.__locastRoomStore`. Needs investigation. |
| permission_e2e:231 | toolbar hidden after DRAW cap is revoked | product failure | After revoke `canDraw` becomes false (the spec's first wait passes) but `DrawingToolbar` is driven only by `keyboard.toolbarVisible`, so an open toolbar stays on screen. |
| permission_e2e:329 | co-host can playback after Co-host preset | stale hook | The spec reads `__locastKeyboardScope.canPlayback`; no such field exists anywhere in `src`. |

No failure was attributed to the browser or the CI environment.

## Infrastructure failures fixed with the CI work

`chat_e2e` (8 tests) and `leave_room_modal_e2e` (2 tests) called
`page.waitFunction`, which is not a Playwright API (`waitForFunction`). Fixing
the name turned 5 of the chat failures green. `tests/playwright` is not covered
by `pnpm typecheck`; running `tsc -p tests/playwright/tsconfig.json` reports
about 30 further errors, so that project is not yet a gate.
