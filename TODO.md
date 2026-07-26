# TODO — tech debt

## Refactor: functions with too many arguments → parameter structs

Several functions carry `#[allow(clippy::too_many_arguments)]` (>7 positional
args). They work correctly — this is a readability/maintainability lint, not a
bug — but positional call sites are error-prone. Refactor each to take a
dedicated parameter struct (e.g. `RenderParams`, `NextEventCtx`, …), then drop
the `#[allow]`.

Sites:
- `src/modes/popup.rs` — `render_html`, `run` (non-windows stub)
- `src/scheduler.rs` — `next_event`
- `src/platform/mod.rs` — `update_or_spawn_replacing_with_env`
- `src/modes/service.rs` — `tick_loop`, `fire_popup_daily`,
  `stamp_daily_fire_success`, and the arm at ~l.2461

Do it as its own pass (touches Windows-live code → re-pack + smoke), not folded
into a feature change.
