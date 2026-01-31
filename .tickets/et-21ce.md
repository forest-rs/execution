---
id: et-21ce
status: closed
deps: [et-9e42, et-2ef3, et-8a8d]
links: []
created: 2026-01-31T01:32:59Z
type: task
priority: 2
assignee: Bruce Mitchener
parent: et-8de9
tags: [execution_tape, tests]
---
# Add conformance tests

Golden bytecode decode/encode tests, verifier rejection cases, and interpreter behavior for loops/recursion/limits.


## Notes

**2026-01-31T06:24:48Z**

Added new workspace crate execution_tape_conformance with internal golden conformance tests: minimal container encoding golden bytes, encode/decode roundtrip + verify + run for pure ops and host_call, verifier rejection of unknown opcode, and VM call-depth trap regression. fmt/clippy/test clean.
