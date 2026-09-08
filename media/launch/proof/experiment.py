"""Designed synthetic refund-retry experiment; Python stdlib only."""

import json

ORDER_ID = "invented-order-001"
ORIGINAL_ID = "refund-operation-001"
REPLACEMENT_ID = "refund-operation-002"
AMOUNT_CENTS = 500


class SimulatedTimeout(TimeoutError):
    """The ledger committed, but its response was lost."""


class RefundLedger:
    """In-memory upstream; operation_id is its only deduplication key."""

    def __init__(self):
        self.refunds = {}

    def refund(self, operation_id, order_id, amount_cents, *, lose_response=False):
        if operation_id not in self.refunds:
            self.refunds[operation_id] = {
                "operation_id": operation_id,
                "order_id": order_id,
                "amount_cents": amount_cents,
            }
        if lose_response:
            raise SimulatedTimeout("Simulated response loss after commit")
        return dict(self.refunds[operation_id])


def run_strategy(retain_original_id):
    ledger = RefundLedger()
    operation_ids = [ORIGINAL_ID]
    timeouts = 0
    try:
        ledger.refund(ORIGINAL_ID, ORDER_ID, AMOUNT_CENTS, lose_response=True)
    except SimulatedTimeout:
        timeouts += 1
        retry_id = ORIGINAL_ID if retain_original_id else REPLACEMENT_ID
        operation_ids.append(retry_id)
        ledger.refund(retry_id, ORDER_ID, AMOUNT_CENTS)
    return {
        "strategy": "retain_original_id" if retain_original_id else "fresh_id",
        "order_id": ORDER_ID,
        "intended_refund_count": 1,
        "intended_amount_cents": AMOUNT_CENTS,
        "attempt_operation_ids": operation_ids,
        "attempt_count": len(operation_ids),
        "retry_count": len(operation_ids) - 1,
        "simulated_timeout_count": timeouts,
        "refund_count": len(ledger.refunds),
        "refunded_amount_cents": sum(r["amount_cents"] for r in ledger.refunds.values()),
    }


def main():
    print(json.dumps({
        "experiment": "designed_synthetic_refund_retry",
        "results": [run_strategy(False), run_strategy(True)],
    }, sort_keys=True))


if __name__ == "__main__":
    main()
