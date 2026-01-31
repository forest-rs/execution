---
id: et-e5bb
status: open
deps: []
links: []
created: 2026-01-31T07:57:32Z
type: task
priority: 2
assignee: Bruce Mitchener
---
# Higher-order functions: const_func + call_indirect (v2?)

We currently support helpers via static calls (`call func_id`) and can punt on higher-order functions without any container/encoding changes.

This ticket tracks adding *first-class function values* (higher-order programming):
- materialize a function reference into a register value (`const_func dst, func_id`)
- call via a register-held callee (`call_indirect eff_out, func_reg, eff_in, argc, args..., retc, rets...`)

Motivation:
- pass/return helper functions
- store callbacks in aggregates
- allow front-ends to compile lambdas/closures (likely with capture via aggregates + explicit env passing)

Open questions:
- Should `ValueType::Func` carry any signature info, or stay opaque + rely on verifier/static typing at call sites?
- Should `call_indirect` take an explicit `FunctionSigId`/type immediate for verification, or infer from `argc`/`retc`?
- How do we represent captured environments (explicit extra arg vs pair of (func, env))?
- Do we need `eq`/hash/ordering semantics for function values?

Non-goals (for v1):
- tail-call optimization
- closures with implicit capture
- async/await
