# Designed synthetic refund-retry experiment

This is a designed synthetic experiment, not a production incident, a benchmark
of AI quality, or a payment-ready implementation. It uses Python stdlib only,
with no network, real payment integrations, or external data.

Each strategy starts with a separate empty in-memory upstream ledger, one
invented order (`invented-order-001`), and one intended refund of 500 cents.
The first call uses `refund-operation-001`, commits, and then raises a
`SimulatedTimeout` to model one lost response. The caller makes exactly one
retry, with either `refund-operation-002` or the original operation ID.
All identifiers and fault placement are fixed; there is no randomness or clock.
The ledger deduplicates solely by operation ID, not order or amount.

## Observed results

Executed `python3 experiment.py`; its JSON output reported:

| Strategy | Attempts | Lost responses | Retries | Refund count | Refunded cents |
| --- | ---: | ---: | ---: | ---: | ---: |
| `fresh_id` | 2 | 1 | 1 | 2 | 1000 |
| `retain_original_id` | 2 | 1 | 1 | 1 | 500 |

Executed `python3 -m unittest -v`: all 4 tests passed. Tests check ledger
commit and deduplication, committed state after response loss, recovery by
retry, both strategy outcomes, and identical machine-readable output across
two executions within the test.

## Resulting retry contract

Assign one stable operation ID to one intended refund before its first attempt.
Retain that ID and the same payload for every retry, including after a timeout.
A timeout leaves the caller uncertain whether the operation committed; it is
not evidence that the refund failed. A fresh ID denotes a new operation to this
ledger, so using it to retry the same intent produces a duplicate refund here.
Reusing the original ID retrieves the committed result without adding a refund.

The demonstrated contract assumes the ledger retains its deduplication state.
This single-process model omits persistence, concurrency, key expiry, validation,
and payload-conflict handling. For an existing ID it returns the stored record;
callers must keep the payload unchanged. The measured result is limited to the
specified post-commit response loss and one retry; it establishes no broader
production reliability or performance claim.
