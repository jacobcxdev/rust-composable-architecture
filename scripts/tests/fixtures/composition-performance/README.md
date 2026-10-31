# Composition performance test fixtures

These files are regression inputs for the performance evaluator and runner.
They are not current benchmark results or release evidence.

`raw/` contains the 19 historical transcripts required by the tests:

- ten Divan process logs, used to check the complete process set and parsing;
- four compile logs, used to check compile measurements;
- allocation, binary-size and environment records; and
- two zero-cost comparison records.

The binary-size fixture records the original unstripped executables. Tests
require all six binary gates to reject those sizes against the corrected
budgets. Keep that failing input: replacing it with passing results would
remove the regression check.

`composition-protocol-v3.json` is an obsolete protocol used to verify that
the runner rejects a receipt from a different protocol. It is not a supported
capture method.

The files were moved without changing their contents from the development
capture archive. Their old version labels and source hashes identify the
original inputs, not a published RCA version. New captures belong under an
ignored output directory, not here.

The maintained protocol, frozen budgets and three hash-checked binary
correction receipts live in
[`docs/performance/composition`](../../../../docs/performance/composition/README.md).
