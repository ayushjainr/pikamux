# Focused test-declaration ledger

R = retain: a plausible product defect changes an independently observed result. F = fix: the asserted behavior is real but the current oracle or harness is weak. D = delete: no independent regression value remains. File and line locations refer to the audited worktree, not a frozen release tag.

| Test | Decision | Credible regression / independent oracle |
| --- | --- | --- |
| `tests/setup_contract.rs:39` OpenCode structured errors | R | Fake plugin execution checks emitted human explanation while secret fields never reach the hook payload. |
| `tests/setup_contract.rs:77` JSON hooks merge | R | Applied hooks retain foreign handlers, quote the executable, and remain idempotent. |
| `tests/setup_contract.rs:107` stale Python/Claude hooks | R | Migration removes obsolete handler, preserves model, and reenables then detects hooks. |
| `tests/setup_contract.rs:128` Codex TOML merge | R | Existing comments/keys survive; invalid TOML is rejected before planning. |
| `tests/setup_contract.rs:151` Pika config merge | R | Unknown user fields and order survive while requested settings change. |
| `tests/setup_contract.rs:170` malformed JSON | R | Invalid input fails without a file write. |
| `tests/setup_contract.rs:187` installed OpenCode event plugin | F→R | Replaced source greps with fake-provider execution of the installed plugin; checks native title write/readback, exact root/child rules, attention mapping, and honest failure. Temporary wrong-title mutation was caught. |
| `tests/setup_contract.rs:298` stale setup preview | R | A stale second file prevents any write, including the valid first file. |
| `tests/setup_contract.rs:330` symlink target | R | A symlink never causes the external target to be overwritten. |
| `tests/setup_contract.rs:352` private backups | R | Apply creates private backups retaining exact prior bytes despite name collision. |
| `tests/setup_contract.rs:388` bounded version probe | R | Fake child holding pipes is reaped within the deadline. |
| `tests/setup_contract.rs:421` probe stderr/status | R | Failed probe preserves its true exit status and stderr. |
| `tests/setup_contract.rs:454` duplicate targets | R | Invalid batch is rejected before write. |
| `tests/setup_contract.rs:481` full preview/apply | R | Planning is read-only; approved apply is idempotent. |
| `tests/differential.rs:224` attention literals | R | Production projection is compared to frozen, independently sourced status fixtures. |
| `tests/differential.rs:231` identity literals | R | Production name resolution is compared to frozen UUID/collision fixtures. |
| `tests/differential.rs:238` fixture route hygiene | R | Frozen fixtures cannot accidentally embed host routes or live-state paths. |
| `tests/differential.rs:256` fixture literal coverage | R | Fixture drift cannot silently erase key attention states or change the public safety-kind spelling. |
| Former `tests/differential.rs::fixture_negative_mutants_are_rejected` | D | It mutated an already-computed output and only verified that a JSON equality helper detects inequality. Production mutants were never run; the real fixture comparisons above remain. |
| `tests/third_party_notices_contract.rs:2` known notice sections | R, renamed | Known required legal sections survive a bad generator even if its changed output is regenerated consistently. It is a limited sentinel, not complete compliance proof. |

The OpenCode fix still uses synthetic provider interfaces; it does not claim live OpenCode compatibility. The temporary mutation modified `assets/pika-opencode.js.in` only within this isolated worktree, failed the new test on the wrong title, and was fully reverted; the asset has no residual diff.
