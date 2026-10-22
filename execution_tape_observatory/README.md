# Execution Tape Observatory

A terminal explorer for real, owned Execution Tape recordings. The optional `std` UI borrows a
recording; capture, immutable value ownership, completeness and replay remain in
`execution_tape_recording`. The core VM and recorder have no terminal dependencies.

From the workspace, run:

```sh
cargo run -p execution_tape_wind_tunnel --bin observatory
cargo run -p execution_tape_wind_tunnel --bin observatory -- --snapshot
```

The example reads three fixed samples (7, 5, 9) through `read_sample`, doubles each in guest
code, and passes each `(index, sample, doubled)` tuple to `store_result`. It returns their sum,
42. The six real host calls are captured before the original VM and host state are dropped.
The timeline connects each call ordinal to its import,
instruction and arguments/returns. Open aggregates to inspect immediate fields and their stable
recording-local identities. The source pane displays opcodes and source register IDs. It does
not claim instruction/register value history.

Use arrows or `j`/`k` to navigate, `Tab` to focus values, `Enter` to open an aggregate/closure
environment, and `Backspace` to return to the previous field. `PageUp`/`PageDown` move by a screen;
`Home`/`End` jump to the first/last item in the focused pane. `r` runs
the existing checked replay in a fresh VM without invoking the live host; `q` quits. Captures
marked partial remain inspectable and replay reports their fault. Text and payload previews are
bounded, graph navigation is iterative, and opaque typed error payloads are never formatted.

Embed the UI with `execution_tape_observatory::explore(&recording)`. This owns raw mode and the
alternate screen until return/unwind and restores both without changing the global panic hook.
`render_text(&recording, width, height)` renders the same initial view for headless review; it
does not perform replay. The interactive view needs at least 80 columns and 20 rows.

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
