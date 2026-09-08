"""Tests of the designed synthetic experiment."""

import json
import unittest
from contextlib import redirect_stdout
from io import StringIO

from experiment import (
    AMOUNT_CENTS, ORDER_ID, ORIGINAL_ID, REPLACEMENT_ID,
    RefundLedger, SimulatedTimeout, main, run_strategy,
)


class LedgerTests(unittest.TestCase):
    def test_commit_and_deduplicate_by_operation_id_only(self):
        ledger = RefundLedger()
        first = ledger.refund(ORIGINAL_ID, ORDER_ID, AMOUNT_CENTS)
        self.assertEqual(first, {
            "operation_id": ORIGINAL_ID, "order_id": ORDER_ID,
            "amount_cents": AMOUNT_CENTS,
        })
        self.assertEqual(ledger.refund(ORIGINAL_ID, ORDER_ID, AMOUNT_CENTS), first)
        self.assertEqual(len(ledger.refunds), 1)
        ledger.refund(REPLACEMENT_ID, ORDER_ID, AMOUNT_CENTS)
        self.assertEqual(len(ledger.refunds), 2)

    def test_lost_response_occurs_after_commit_and_retry_recovers(self):
        ledger = RefundLedger()
        with self.assertRaises(SimulatedTimeout):
            ledger.refund(ORIGINAL_ID, ORDER_ID, AMOUNT_CENTS, lose_response=True)
        self.assertEqual(len(ledger.refunds), 1)
        committed = dict(ledger.refunds[ORIGINAL_ID])
        self.assertEqual(ledger.refund(ORIGINAL_ID, ORDER_ID, AMOUNT_CENTS), committed)
        self.assertEqual(len(ledger.refunds), 1)


class StrategyTests(unittest.TestCase):
    def test_strategy_outcomes(self):
        for retain, count, retry_id in [(False, 2, REPLACEMENT_ID), (True, 1, ORIGINAL_ID)]:
            with self.subTest(retain_original_id=retain):
                result = run_strategy(retain)
                self.assertEqual(result["refund_count"], count)
                self.assertEqual(result["refunded_amount_cents"], count * AMOUNT_CENTS)
                self.assertEqual(result["intended_refund_count"], 1)
                self.assertEqual(result["attempt_count"], 2)
                self.assertEqual(result["retry_count"], 1)
                self.assertEqual(result["simulated_timeout_count"], 1)
                self.assertEqual(result["attempt_operation_ids"], [ORIGINAL_ID, retry_id])

    def test_output_is_machine_readable_and_deterministic(self):
        outputs = []
        for _ in range(2):
            stream = StringIO()
            with redirect_stdout(stream):
                main()
            outputs.append(stream.getvalue())
        self.assertEqual(outputs[0], outputs[1])
        observed = json.loads(outputs[0])
        self.assertEqual(observed["experiment"], "designed_synthetic_refund_retry")
        self.assertEqual(observed["results"], [run_strategy(False), run_strategy(True)])


if __name__ == "__main__":
    unittest.main()
