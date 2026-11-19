"""Offline tests of the runner: parsing and verdicts only, no Zorro, no
network.

    python -m unittest discover -s tests/zorro
"""

import tempfile
import unittest
from pathlib import Path

import zorrorun as z

# Shaped like a real stress test run; every figure is made up.
WINDOW = """\
!Hyperliquid plugin loaded (liquidzorro)
HyperliquidPerpStressTest compiling............
Login 0 Hyperliquid..
!Hyperliquid: logging in (testnet)...
!Hyperliquid: login OK (testnet), address=0x1111111111111111111111111111111111111111
""".splitlines()

LOG = """\
=== Hyperliquid perpetual stress test ===
  PASS  BrokerAccount reports a positive account value
  PASS  The price history has a bar for every minute
!BrokerBuy2: buy ETH FAILED: post-only order refused, it would have crossed the book
  PASS  A crossing post-only order is refused
=== RESULT: 3 passed, 0 failed ===
Logout..
!Hyperliquid: logged out ok
""".splitlines()


def run(window=WINDOW, log=LOG):
    return z.combine(z.parse(window), z.parse(log) if log is not None else None)


class Verdict(unittest.TestCase):
    def test_a_clean_run(self):
        out = run()
        self.assertEqual(out.network, "testnet", "the login is only in the window")
        self.assertEqual(out.result, (3, 0))
        self.assertEqual(len(out.passed), 3, "checks counted once, from the log")
        self.assertEqual(len(out.refusals), 1, "the provoked refusal is listed, not failed")
        self.assertEqual(z.verdict(out, 3), (z.EXIT_PASS, []))
        self.assertEqual(z.verdict(out, None), (z.EXIT_PASS, []))

    def test_a_wrong_count_fails(self):
        code, reasons = z.verdict(run(), 4)
        self.assertEqual(code, z.EXIT_FAIL)
        self.assertIn("3 passed, expected 4", reasons)

    def test_a_failed_check_fails(self):
        out = run(log=LOG[:2] + ["  FAIL  Account ends flat", "=== RESULT: 1 passed, 1 failed ==="])
        self.assertEqual(out.failed, ["Account ends flat"])
        self.assertEqual(z.verdict(out, None)[0], z.EXIT_FAIL)

    def test_mainnet_anywhere_wins(self):
        out = run(window=["!Hyperliquid: logging in (mainnet)..."])
        self.assertEqual(out.network, "mainnet")
        self.assertEqual(z.verdict(out, 3)[0], z.EXIT_MAINNET)

    def test_no_result_line_is_incomplete(self):
        out = run(log=["Subscribe ETH-PERP", "Error 053: ETH-PERP currently unavailable", "Logout.."])
        code, reasons = z.verdict(out, None)
        self.assertEqual(code, z.EXIT_INCOMPLETE)
        self.assertTrue(any("Error 053" in r for r in reasons), reasons)

    def test_no_log_uses_the_window(self):
        out = run(window=WINDOW + LOG, log=None)
        self.assertEqual(z.verdict(out, 3), (z.EXIT_PASS, []))

    def test_a_failed_login_is_named(self):
        out = run(window=["!Hyperliquid: login FAILED: no password supplied; mainnet needs the key in the Password field"], log=[])
        self.assertIn("mainnet needs the key", out.login_error)
        self.assertEqual(z.verdict(out, None)[0], z.EXIT_INCOMPLETE)

    def test_a_result_that_disagrees_with_the_lines(self):
        out = run(log=LOG[:2] + ["=== RESULT: 5 passed, 0 failed ==="])
        code, reasons = z.verdict(out, None)
        self.assertEqual(code, z.EXIT_FAIL)
        self.assertTrue(any("the log lists" in r for r in reasons), reasons)


class Cargo(unittest.TestCase):
    def test_results_are_summed(self):
        text = "test result: ok. 9 passed; 0 failed; 2 ignored\ntest result: ok. 1 passed; 0 failed; 0 ignored"
        self.assertEqual(z.cargo_verdict(0, text), (z.EXIT_PASS, ["10 passed"]))

    def test_a_failure_or_no_test_fails(self):
        self.assertEqual(z.cargo_verdict(101, "test result: FAILED. 8 passed; 1 failed;")[0], z.EXIT_FAIL)
        self.assertEqual(z.cargo_verdict(0, "test result: ok. 0 passed; 0 failed;")[0], z.EXIT_FAIL)
        self.assertEqual(z.cargo_verdict(101, "error[E0425]: cannot find value")[0], z.EXIT_INCOMPLETE)


class Hygiene(unittest.TestCase):
    def test_keys_and_addresses_are_redacted(self):
        key, address = "0x" + "ab" * 32, "0x" + "cd" * 20
        self.assertEqual(z.redact(f"key {key} address={address}"), "key 0x<key> address=0x<address>")

    def test_env_file(self):
        with tempfile.TemporaryDirectory() as d:
            env = Path(d) / ".env"
            env.write_text('# comment\nexport HL_TESTNET_PRIVKEY="0xabc"\nZORRO_DIR=C:\\Z\nEMPTY=\n', encoding="utf-8")
            self.assertEqual(z.read_env(env), {"HL_TESTNET_PRIVKEY": "0xabc", "ZORRO_DIR": "C:\\Z"})
            self.assertEqual(z.read_env(Path(d) / "missing"), {})


class Stages(unittest.TestCase):
    def test_stage_names(self):
        self.assertEqual(z.parse_stage("perp:58"), ("zorro", "HyperliquidPerpStressTest", 58))
        self.assertEqual(z.parse_stage("spot"), ("zorro", "HyperliquidStressTest", None))
        self.assertEqual(z.parse_stage("MyScript:3"), ("zorro", "MyScript", 3))
        self.assertEqual(z.parse_stage("unit"), ("unit", "cargo test", None))
        self.assertEqual(z.parse_stage("live:exports_the_asset_list"), ("live", "exports_the_asset_list", None))


if __name__ == "__main__":
    unittest.main()
