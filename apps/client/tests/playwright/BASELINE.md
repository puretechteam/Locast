# Playwright baseline

CI runs the Playwright suite as two jobs (`.github/workflows/ci.yml`):

- `e2e gate`: hard gate. Runs the spec files listed in the `test:e2e:gate`
  script in `apps/client/package.json`. Any failure fails CI.
- `e2e full-suite baseline`: non-gating. Runs every spec, lists every failing
  test in the run summary and as warning annotations, and uploads the report,
  traces and videos.

When a spec fails, add a row for it below (spec, test, class, evidence). When it
is fixed, add its file to `test:e2e:gate` (when the whole file is green) and
delete its row here.

Nothing is skipped, deleted or loosened. The baseline run had 121 tests, 12 of
which failed. All 12 have since been fixed and their rows removed, and every
spec file is now in the gate, so no failing test is recorded.

| Spec | Test | Class | Evidence |
| --- | --- | --- | --- |
| (none) | | | |

The last row to go was `host_disconnect_grace_e2e:66`. It was a product defect:
`RoomTopBar` armed its "Room ended" toast only from `room://state` events seen
after it mounted, so a room hydrated from the store never showed it, and its
effect never unsubscribed from the event.

No failure was attributed to the browser or the CI environment.

## Infrastructure failures fixed with the CI work

`chat_e2e` (8 tests) and `leave_room_modal_e2e` (2 tests) called
`page.waitFunction`, which is not a Playwright API (`waitForFunction`). Fixing
the name turned 5 of the chat failures green. A type check would have caught
it, so `tests/playwright` is now part of `pnpm typecheck`
(`tsc -p tests/playwright/tsconfig.json`).
