---
id: et-de56
status: closed
deps: []
links: []
created: 2026-01-31T03:50:00Z
type: task
priority: 1
assignee: Bruce Mitchener
parent: et-8de9
tags: [execution_tape, bytecode, api]
---
# ProgramBuilder: declare/define functions + build_checked

Extend `ProgramBuilder` with declare/define workflow so callers can assemble cross-calling functions without hardcoding indices; add `build_checked` to verify full program (including cross-function call checks).


## Notes

**2026-01-31T03:50:04Z**

Implemented declare/define functions, `build_checked(+_with)`, `BuildError`, and `Asm::call_func`; added test for cross-call verification.
