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
being different numbers is the point of the surface: 53 things worth a look and
zero things that gate is a green board with a long report, and a dashboard that
only has one number cannot say that.

This is also the one surface where the internal names belong. `bind.mcu_bound`
is exactly right here and exactly wrong in the plain report, which scenario 01
checks from the other side.

## Determinism

The same board twice must give byte-identical JSON. If it does not, no dashboard
can diff two builds, and every commit looks like it changed something. This is
asserted by running it twice and comparing, not by reasoning about it.

## The JUnit

Well-formed XML with a `testsuites` root, real `testsuite` and `testcase`
elements, and counts that agree with the verdict. The failing spec's report has
`failures="1"` while the process exits 1; a green one has `failures="0"`. A
JUnit file claiming an all-green suite next to a red exit code is the single
worst thing this surface can do, because the CI provider believes the file, not
the exit code, and the build looks green in the UI while the pipeline is red.

The runner refuses a JUnit file carrying a DOCTYPE or an entity declaration. A
legitimate report has neither, and parsing untrusted XML with entity expansion
turned on is how a test-report file becomes an exploit.
