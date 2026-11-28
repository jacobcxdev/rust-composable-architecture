# rca-test

Add this crate as a dev-dependency to test RCA features without a real clock or network.
`TestStore` checks state changes and received actions. Controlled futures, streams,
and gates let the test decide when an external input arrives. Enable `local`
for owner-thread tests with non-`Send` values.

Read the [testing guide](../../docs/guides/testing.md).
For a published version, select it in the [API reference](https://docs.rs/rca-test).
