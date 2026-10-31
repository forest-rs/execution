"Copyright 2026 the Execution Tape Authors
 SPDX-License-Identifier: Apache-2.0 OR MIT"

run
    | counter left right first second answer escaped late |
    counter := Cell with: 40.
    left := self makeIncrement: counter.
    right := self makeIncrement: counter.
    first := left value: 1.
    second := right value: 1.
    answer := self answerUsing: counter.
    escaped := self makeEscaped: counter.
    late := escaped value.
    ^ { first. second. answer. late }

makeIncrement: counter
    ^ [ :amount | counter value: (counter value + amount) ]

answerUsing: counter
    | answer |
    answer := [ [ ^ counter value ]
        ensure: [ Demo wait. counter cleaned: 2 ] ]
        ensure: [ counter cleaned: 1 ].
    Demo afterEnsure.
    ^ answer

makeEscaped: counter
    ^ [ ^ counter value ]
