# Execution Tape Observatory

A terminal browser for compiled Execution Tape programs, live invocations and real, owned recordings. The optional `std` UI borrows a
recording; capture, immutable value ownership, completeness and replay remain in
`execution_tape::recording`. The core VM and recorder have no terminal dependencies.

Try live execution with real host objects and authored source:

```sh
cargo run -p execution_tape_wind_tunnel --bin live_debugger -- --color=always
cargo run -p execution_tape_wind_tunnel --bin live_debugger -- --snapshot
cargo run -p execution_tape_wind_tunnel --bin live_debugger -- --snapshot --complete
```

The live demo opens inside `double`, before its first host call. Its caller `main` is waiting
for that call to return. `Counter.read` reads a real host-owned receiver; `Counter.publish`
records the result. Source snapshots show the small example's exact assembly-to-source mappings;
the example assembles this source directly rather than providing a language compiler.
Use `--steps N` to select another initial instruction boundary.

`s` steps into a guest call; `n` steps over; `u` steps out of the active leaf, even when a caller
is selected for inspection. `c` continues, Space pauses, `b` toggles an instruction breakpoint,
and `g` selects the current instruction. Breakpoints stop **before** executing the instruction;
continuing from one intentionally passes that breakpoint once. Stepping over/out also honors
breakpoints inside callees. `Tab`/`Shift+Tab` cycle the live stack, instructions, values and authored
source. Focus uses a green double border. `Enter` opens the first visible value's aggregate or
closure environment, or follows a host object through an attached inspector; `Backspace` goes up.
The `▸` row in Values is the value Enter will open. Values and source scroll with arrows or `j`/`k`.

Invocations started with `InvocationOptions::yield_host_calls()` show **AWAITING HOST** at an
admitted request. Stepping/continuing does not silently invoke it. `h` explicitly dispatches
through the embedding's existing host and finishes that instruction, including its real effects.
`LiveExplorer::into_invocation` leaves presentation while preserving the pending handle for an
external or asynchronous completion. Dropping an unfinished explorer still cancels its guest.

When a host request initiates another invocation, attach
`LiveExplorer::requested_by(&request, Some(&caller_sources))`. The stack gains a selectable
**external caller** row. Select it to inspect the requesting instruction, its exact authored
source, and bounded arguments/initialized registers. Caller register analysis is built on first selection;
unreturned destinations remain unavailable. Caller and child maps are validated independently;
overlapping local function IDs do not imply the same code. Foreign handles are not opened in
the child's heap or host. This is one causal link across independent invocations, without a
synthetic guest activation or a scheduler-wide task graph. Stepping still controls the child;
breakpoints in the external caller belong to its own invocation. `g` returns to current code.

Attach `LiveExplorer::object_inspector` with an `ObjectInspector<H>` (or a closure) to project
actual host state as an `ObjectView`. It runs on demand while an object view is open, between
execution quanta. It must be read-only, respect the requested field limit before materializing
fields, and bound payload previews at their source. The UI escapes text and displays at most
256 fields. Scalars and host object references are supported; VM-local aggregate/callable
handles from this adapter have no owner and are explicitly unavailable. An absent projection
does not imply an empty or collected object. Browsing never retains objects or adds GC roots.
The live header names the actual prepared program and reports cumulative aggregate/payload
work, including owned-capture import work. Recorded evidence cannot inspect later live host state.

The `▸` instruction marker is the next instruction for a leaf or the pending call for a waiting
caller; the highlighted browsing row is independent. `●` marks a breakpoint. Caller return
destinations remain unavailable until results arrive, unless they held an earlier initialized
value. Unavailable values are never presented as zero or false. `o` switches to conservative
host-object references, including VM aggregate fields; this is not a minimal liveness set or
a complete root set for an embedding language. The host still owns its objects and collection.

Embed `LiveExplorer::new(vm.start(...)?).sources(&source_map)?` with your actual host and program.
`run(&mut self)` leaves the invocation available on return; dropping an unfinished explorer cancels
guest work. `step`, bounded `resume`, `result`, `take_outputs` and `render_text` also work without
terminal modes. Direct `resume` uses VM quanta and ignores UI breakpoints. Hard resource budgets
accumulate across quanta. Continuation polls input between bounded batches; host calls are
synchronous and indivisible and can block input. **Host effects are real and are not rolled back.**
Trace scopes measure logical elapsed latency, including time paused. This does not edit registers,
replace running code, rewind effects or reconstruct a post-trap frame. Traps show their actual
location and clear live values; recordings remain the separate route to retained evidence.

The program-file browser below opens arbitrary tapes without a host; executing them requires
the embedding's actual bindings and state.

From the workspace, run:

```sh
cargo run -p execution_tape_wind_tunnel --bin observatory -- --color=always
cargo run -p execution_tape_wind_tunnel --bin observatory -- --snapshot
```

Open a compiled tape without executing it or loading a host:

```sh
cargo run -p execution_tape_wind_tunnel --bin observatory -- --program path/to/program.tape --color=always
cargo run -p execution_tape_wind_tunnel --bin observatory -- --program path/to/program.tape --snapshot
```

To try the file browser with a self-contained artifact, write the demo program to a **new** file:

```sh
mkdir -p .local
cargo run -p execution_tape_wind_tunnel --bin observatory -- --write-demo .local/demo.tape
cargo run -p execution_tape_wind_tunnel --bin observatory -- --program .local/demo.tape --color=always
```

The file browser verifies the decoded program once. Rejected programs remain inspectable and
open at the reported function/instruction when one is available; `d` returns to that diagnostic.
Program-wide failures do not invent an instruction location. A file that cannot be decoded reports
an error before terminal mode starts. Files are limited to 64 MiB of encoded input; decoded data and
verification use additional memory. Successfully displaying a rejected program is a successful
browser invocation; both the program header and the Details panel title explicitly say
`VERIFIED` or `REJECTED`. Details starts with the outcome and, for rejection, the reason.

Use `[` and `]` to switch between functions, imports, constants and aggregate types. `/` searches
visible labels (case sensitive); `Enter` applies the filter, and `Esc` clears it. Select a function,
then `Enter` focuses its instructions. In the instruction pane, `Enter` follows a direct guest call,
host import, jump or true branch arm; `f` follows the false arm. `Backspace` returns to the exact
previous instruction. Indirect runtime callees are explicitly unknown. The last 256 followed
locations are retained. `Tab`/`Shift+Tab` cycle the navigator, instructions and scrollable details;
arrows, page keys and `Home`/`End` operate on the focused pane. `q` quits.

The details pane now shows verifier-backed **before → after** contracts for the selected
instruction's operand registers, with destinations first. Aliased projections show the old
aggregate and its projected result separately. Local origins name an argument, defining opcode/PC,
or incoming block state. At joins, inspect both static input arms and their simultaneous parameter
bindings beside the common guaranteed fact; differing tuple-member kinds appear as `?`.
This means the member type is unknown, rather than a verification error. For example,
`tuple(i64)` and `tuple(bool)` can join as `tuple(?)`: both have one element, and `TupleLen`
does not require its type.

These are source dataflow facts: call outputs assume successful callee return, and branch outputs
precede their outgoing edge bindings. Unreachable instructions carry no value guarantees. Rejected
programs keep their diagnostic view and explicitly report inferred facts unavailable. Analysis is
created for the selected function and instruction views are cached across repaints. Rendering
shows at most four value-register facts and four incoming arms/bindings at a time, with explicit
omissions and operand scan limits; no VM executes while browsing.

The details pane shows compositional guest signatures (with bounded nested contract previews), selected operand register declarations, constant previews and
aggregate layouts. Call destinations show their nested result contracts on successful return;
return instructions show the required contracts. Register declarations are labelled **storage
kinds**: an `aggregate` register can hold a nested tuple whose structure is described by those
contracts. These boundary requirements are separate from runtime values and from inferred
register facts at other instructions. Rejections name the function, expected/found types and a
readable member path (including struct field names when available). Span identifiers
remain opaque. Explicit frontend source maps connect instruction locations to snapshots; no numeric span convention is assumed. Previews and large operand/field
lists have explicit bounds. The terminal adapter enables Ratatui's `unstable-rendered-line-info`
feature to scroll details using their actual wrapped height; no terminal dependency enters the VM.

Embed program browsing with `browse(&program)` or render it headlessly with
`render_program(&program, width, height)`. Existing recording entry points keep their behavior.

The example reads three fixed samples (7, 5, 9) through `read_sample`, doubles each in guest
code through the guest function `read_and_double`, and passes each `(index, sample, doubled)` tuple to `store_result`. It returns their sum,
42. The six real host calls are captured before the original VM and host state are dropped.
The initial view selects the first host call and shows the final guest result above it.
Recorded events are the invocation, host calls and completion; guest instructions execute between
these events. The selected call names its host function and signature. One call-values pane shows
blue `→ Argument` rows sent to the host and green `← Return` rows received before VM signature
checks. Values start with their contents; aggregate IDs are secondary. Open a tuple, array, struct
or closure environment to inspect its immediate fields.

The application enters `double_and_sum` through the VM; in this demo the Rust `demo_recording`
function calls `Vm::run`. The header and first event show the application-to-guest invocation with
its captured inputs. This boundary sits outside the guest stack; the application's native Rust
call stack is not captured.

The guest stack pane shows actual recorded activations, innermost first. A read call has
`read_and_double` above `double_and_sum`; a store call has only `double_and_sum` after the helper
has returned. Focus the stack and select a caller to move source to its suspended call instruction.
Stacks contain guest frames; missing observations are shown explicitly.

The source pane shows named call operands and guest registers. A colored gutter connects static branch sources and targets in independent lanes. `T` and `F`
label conditional arms; `▶` marks a target, and `↑`/`↓` indicate lines continuing outside the
viewport. Self-loops use `↺`. Four lanes keep the display bounded; excess edges are explicitly
counted as hidden. These lines show possible control flow, not every taken branch. Source registers are operands, not captured register-value history. Syntax colors distinguish
registers, literals, operations and callees; value contents stand out from secondary type labels.
The palette uses indexed xterm-256 colors without requiring 24-bit RGB support. The demo honors
`NO_COLOR` by default; `--color=always` explicitly overrides it for the interactive viewer.

Use arrows or `j`/`k` to navigate. `Tab` cycles events, values and stack (`Shift+Tab` reverses); a bold green
double border and `ACTIVE` title mark the keyboard target. Use `Enter` to open an aggregate/closure
environment, and `Backspace` to return to the previous field. `PageUp`/`PageDown` move by a screen;
`Home`/`End` jump to the first/last item in the focused pane. `r` runs
the existing checked replay in a fresh VM without invoking the live host; `q` quits. Captures
marked partial remain inspectable and replay reports their fault. Text and payload previews are
bounded, graph navigation is iterative, and opaque typed error payloads are never formatted.

Embed the UI with `execution_tape_observatory::explore(&recording)`. This owns raw mode and the
alternate screen until return/unwind and restores both without changing the global panic hook.
`render_text(&recording, width, height)` renders the same initial view for headless review; it
does not perform replay. The interactive view needs at least 80 columns and 20 rows.

To browse nested guest results through the ordinary pipeline, create a new example tape:

```sh
cargo run -p execution_tape_wind_tunnel --bin nested_contracts -- .local/nested.tape
cargo run -p execution_tape_wind_tunnel --bin observatory -- --program .local/nested.tape
```

The producer returns `((42), true)`; the caller projects the inner integer. Guest signatures
show those nested contracts, while register declarations continue to show storage kinds.

To inspect how a typed join retains common structure while losing a member's scalar kind:

```sh
cargo run -p execution_tape_wind_tunnel --bin join_contracts -- .local/join.tape
cargo run -p execution_tape_wind_tunnel --bin observatory -- --program .local/join.tape
```

Focus instructions, select `BrArgs` and press `Enter` to follow the join. Both static arms target
one parameter: one supplies `tuple(i64)`, the other `tuple(bool)`. The header shows the common
`tuple(?)` fact and both original bindings. The guest can still compute its length; the demo
executes both conditions and checks that each returns one.

Licensed under either of Apache License, Version 2.0 or MIT license at your option.

Press `i` in the recording explorer to switch between boundary events and actual instruction
attempts. Arrow keys navigate retained execution order; the source and guest stack follow the
selected attempt. The detail row shows its actual successor and outcome. Watched registers show
**before** inputs and **after** successful local writes; Enter opens observed aggregates.
A separate verifier panel shows static before/after guarantees when screen space permits.
Its call-output facts assume a successful return and are distinct from dispatch-time observations.
History completeness is labelled separately from boundary capture, including truncated prefixes.
The built-in recording demo enables a bounded history and explicit operand watches.

## Frontend source connections

Use one configured browser for interactive and headless views:

```rust,no_run
# fn example(program: &execution_tape::program::Program, sources: &execution_tape::source::SourceMap) -> Result<(), Box<dyn std::error::Error>> {
use execution_tape_observatory::ProgramBrowser;
let browser = ProgramBrowser::new(program).sources(sources)?;
let text = browser.render_text(120, 36)?;
# Ok(())
# }
```

For an interactive view, finish the same builder with `run()`. The existing `browse` and
`render_program` functions remain conveniences without source maps. The recording explorer can
reuse the public registry; this browser slice does not infer historical source execution.

A frontend supplies `SourceDocument { name, text }` snapshots and explicit
`SourceMapping { location, document, range }` entries. Locations are exact function/bytecode PCs;
ranges are half-open UTF-8 byte ranges, with empty ranges representing a point. Source names are
labels and never cause filesystem reads. Generated code can remain unmapped. Every selected
instruction shows either its supplied context or an explicit missing-mapping message.

These types live in `execution_tape::source`, a `no_std + alloc` API with no terminal dependency.
Frontends can construct/export maps and headless tools can bind/query them using only the core.
`bound.lookup(location)` returns a validated borrowed range: use `text()`, `document()`, `range()`,
`start()` and `end()` without allocating previews. Positions have explicit `line` and
`byte_column` fields. Observatory consumes the same map and adds terminal-cell layout.

`SourceMap::new` validates documents/ranges, duplicate locations and exact instruction boundaries.
`bind` compares the full canonical `Program::encode()` bytes, including constants, imports and
contracts. Equal decoded programs can share a map; a changed program is rejected before terminal
modes begin. Snapshots are the exact text supplied by the frontend, with no automatic reload from
possibly changed files. Source correctness remains the frontend's responsibility.

Source context is bounded, follows the selected instruction, and caches across repaints. The
source panel shows exact byte coordinates separately from its preview. Tabs, control sequences
and direction-changing Unicode controls are escaped; original source is retained unchanged.
Line/column coordinates are one-based lines and **byte** columns, rather than display-cell or
language-specific columns. Large ranges explicitly continue beyond their visible context.

The standalone `SourceMap::encode`/`decode` sidecar includes supplied source text and one complete
encoded-program identity. Export is explicit and separate from the tape wire format. Default
limits allow 64 MiB of program identity, 16 MiB of source/label bytes, 1,024 documents, one million
line-index entries and one million mappings. These bound used payload/items; decoded program,
indexes, capacities and allocator overhead add memory. The CLI additionally bounds sidecar files
to 128 MiB. It never opens document labels as paths.

Try the small real arithmetic frontend, which parses expressions and exports their exact AST
ranges during emission:

```sh
cargo run -p execution_tape_wind_tunnel --bin source_frontend -- --snapshot
cargo run -p execution_tape_wind_tunnel --bin source_frontend -- --source expression.expr --write-prefix /tmp/expression
cargo run -p execution_tape_wind_tunnel --bin observatory -- --program /tmp/expression.tape --sources /tmp/expression.sources --snapshot
```

The frontend accepts integer literals, parentheses, `+`, `-` and `*`, validates the emitted program,
and executes through the ordinary VM. Its built-in expression returns 24. Input is bounded to
1 MiB and nesting to 128; exports create new files instead of overwriting existing artifacts.

Attach the same source bundle to an owned recording and open directly on captured instructions:

```rust,no_run
# fn example(record: &execution_tape::recording::Recording, sources: &execution_tape::source::SourceMap) -> Result<(), Box<dyn std::error::Error>> {
use execution_tape_observatory::RecordingExplorer;
let explorer = RecordingExplorer::new(record).sources(sources)?.instructions();
let text = explorer.render_text(160, 60)?;
# Ok(()) }
```

Frontend context follows the selected attempt or suspended caller. Missing mappings are labelled;
opening the recording does not infer source ranges or read a source file from disk. Existing
`explore` and `render_text` functions remain conveniences for the default boundary view.
