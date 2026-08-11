#!/usr/bin/env python3
"""Contract tests for the release-required co-simulation runner."""

import sys
import time
import unittest
from pathlib import Path

from run_required_integrations import Gate, evaluate_result, run_command


class RequiredIntegrationEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.gate = Gate(
            name="renode-rp2040-adc",
            command=("cargo", "test"),
            expected_test="rp2040_adc_injection_reaches_firmware",
        )

    def test_zero_exit_skip_is_not_a_pass(self) -> None:
        output = """\
running 1 test
SKIP: Renode not installed
test rp2040_adc_injection_reaches_firmware ... ok
test result: ok. 1 passed; 0 failed
"""

        problems = evaluate_result(self.gate, 0, output)

        self.assertIn("reported SKIP", "\n".join(problems))

    def test_named_real_test_must_be_observed_passing(self) -> None:
        output = "test result: ok. 0 passed; 0 failed; 1 filtered out\n"

        problems = evaluate_result(self.gate, 0, output)

        self.assertIn(self.gate.expected_test, "\n".join(problems))

    def test_nonzero_cargo_status_is_a_failure(self) -> None:
        output = "test rp2040_adc_injection_reaches_firmware ... FAILED\n"

        problems = evaluate_result(self.gate, 101, output)

        self.assertIn("status 101", "\n".join(problems))

    def test_exact_named_pass_without_skip_is_accepted(self) -> None:
        output = """\
running 1 test
test rp2040_adc_injection_reaches_firmware ... ok
test result: ok. 1 passed; 0 failed
"""

        self.assertEqual(evaluate_result(self.gate, 0, output), [])

    def test_named_pass_may_print_nocapture_evidence_before_ok(self) -> None:
        output = """\
running 1 test
test rp2040_adc_injection_reaches_firmware ... ADC counts: low=410 high=3276
firmware observed both injected values
ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
"""

        self.assertEqual(evaluate_result(self.gate, 0, output), [])

    def test_external_command_timeout_is_bounded_and_explicit(self) -> None:
        started = time.monotonic()

        result = run_command(
            (sys.executable, "-c", "import time; time.sleep(30)"),
            Path.cwd(),
            timeout_seconds=0.05,
        )

        self.assertEqual(result.returncode, 124)
        self.assertIn("REQUIRED INTEGRATION TIMEOUT", result.stdout)
        self.assertLess(time.monotonic() - started, 2.0)


if __name__ == "__main__":
    unittest.main()
