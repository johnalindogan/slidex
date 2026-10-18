# Known issues

GitHub issues are disabled on this repo. These are the open ones.

## Flaky cast ready-timeout test

`cast::tests::session_ready_timeout_logs_source_timeout_and_sets_status_error` failed once in pull-request run [39121694895](https://github.com/johnalindogan/slidex/actions/runs/39121694895) and passed on rerun. It panicked at `src/cast.rs:9440` on the connect-failure path: the wall-clock assert that a silent peer times out in about 400 ms (`elapsed >= 350 ms` and `elapsed < 6 s`). The test needs a deterministic clock or an explicit sync instead of measuring `Instant`.

## Empty-selection strip drag moved the slide once

One time, a single strip drag with an empty selection moved the slide on screen by one. App QA could not make it happen again. `slidex-core` tests cover a single drag with nothing selected, by the strip and by the handle, and check that the current slide stays the same.

## Cast panel stays on Connecting…

The cast panel stays on `Connecting…` for about 14 seconds after `cast.log` shows the connection is up.
