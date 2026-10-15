# What a real user should experience

Nobody reads a terminal report in CI. The build engineer needs the same run in
two machine shapes: JSON for a dashboard, JUnit XML for the test-report tab that
the CI provider already renders.

## The JSON

It has to carry `schema_version`, because a consumer that cannot tell which
version it is reading will break silently on the first field rename. It has to
validate against the schema the repository ships, at
`crates/hauksbee-engine/schemas/hauksbee-run-report.schema.json`, because a
schema nothing is checked against is documentation, not a contract.

And it has to carry the rollup the dashboard actually needs: `ok`, `verdict`,
`serious_count`, `actionable_count`. `serious_count` and `actionable_count`
being different numbers is the point of the surface: many things worth a look
and zero confirmed serious findings are not enough to call a run green when a
critical active part has no executable model.

This is also the one surface where the internal names belong. `bind.mcu_bound`
is exactly right here and exactly wrong in the plain report, which scenario 01
checks from the other side. Watchy must report `critical_parts_bound = "5/6"`,
`verdict = "pass"` with a `judgement_scope` naming U1 and a first verdict
condition saying which checks it leaves unjudged, and the unresolved U1/SR2HARU entry must say that the
matched `sr2` card carries identity metadata but no executable behaviour.
Blinky provides the affirmative JSON control: it validates against the same
schema while reporting `ok = true`, `verdict = "pass"`, and one of one critical
active devices executable. This keeps a valid-invalid contract from replacing
the successful export contract.

## Determinism

The same board twice must give byte-identical JSON. If it does not, no dashboard
can diff two builds, and every commit looks like it changed something. This is
asserted by running it twice and comparing, not by reasoning about it.

## The JUnit

Well-formed XML with a `testsuites` root, real `testsuite` and `testcase`
elements, and counts that agree with the verdict. Watchy's JUnit has a
`qualified U1` evidence testcase and no failure, matching its partly judged
pass: the testcase is how a test-report UI shows which checks U1 leaves
unjudged.
Blinky is the genuinely executable green control and has `failures="0"`. The
failing assertion spec separately has `failures="1"` while its process exits 1.
A JUnit file claiming an all-green suite for invalid or failing evidence is the
single worst thing this surface can do, because the CI provider believes the
file, not a terminal explanation.

The runner refuses a JUnit file carrying a DOCTYPE or an entity declaration. A
legitimate report has neither, and parsing untrusted XML with entity expansion
turned on is how a test-report file becomes an exploit.
