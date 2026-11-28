# Reply to the original authorisation request

The [runnable example](../examples/authorisation.rs) mounts an optional detail and
sends its parent a typed authorisation request.
The parent replies to that request through a one-shot sender. It does not look up a
detail by business ID.

```console
cargo run -p rca-example-composition-search --example authorisation
cargo test -p rca-example-composition-search --example authorisation
```

The request owns the Tokio sender. The reply contains non-Clone data, not permission
to change state.
After the reply arrives, the requester calls `update` through the `TaskContext` it
captured before the wait.
The actor accepts the update only for that original installation, even if its
replacement uses the same business ID.

## Two small application bounds

The requester uses a semaphore to allow one active attempt.
If another attempt owns the permit, `try_acquire_owned` returns an error and the
requester reports `Busy` immediately.
The requester keeps the permit through delivery, the reply wait and the state update.
Completion or destruction of its future releases the permit. This limit belongs to the
application, not RCA.

The parent also has room for one request in its inbox. Its synchronous Output handler
calls `try_send` without waiting.
If the inbox is full, the handler replies `Busy`. If the inbox is closed, the handler
passes the output on.
Without a recipient, the requester reports `NoRecipient`.

These limits apply to different things. Cancellation can release the permit while the
old request still occupies the inbox.
Neither limit bounds how many reply handles an external recipient can keep.
The named operation uses `Coexist` so a second attempt cannot replace the first before
the semaphore check.

## Outcomes and ownership

| Event | Result |
| --- | --- |
| No root recipient or closed recipient inbox | `NoRecipient` |
| Active attempt or full recipient inbox | `Busy` |
| Recipient drops an accepted request without replying | `Abandoned` |
| The reply wait expires | `TimedOut` |
| Output admission or the scoped update fails | Original access reason and progress |
| Cancellation or Store shutdown stops a managed request | The runtime drops the waiting task. Its ordinary completion callback need not run. |

The timeout starts **after Output delivery succeeds**. It does not cover admission or
the state update.
The requester holds its permit while it waits for admission. An available reply can
win even with a zero-duration timeout.
The timeout test first establishes that the receiver returned Pending.

The application handles an ordinary result through its report callback. It does not
try another update through a stale context or print the credential.

`Reply::respond` takes the sender at most once. A second call returns the payload from
that call.
If the receiver no longer exists, the first call also returns its original payload.
A successful send transfers ownership. Later cancellation does not return that
payload.
Likewise, an update error does not guarantee recovery after the actor receives the
callback.

## Cancellation, replacement and shutdown

1. While the requester is live, capture its operation wait.
2. Request cancellation.
3. Await the saved wait.

A cancellation acknowledgement alone does not establish that the runtime destroyed the
future. The same distinction applies to Store shutdown.

A replacement detail has a different identity and a new one-shot receiver. The old
sender cannot answer its request.
A caller can retain an old requester context outside the managed task. Even after a
late reply, that context cannot update the replacement.

The example uses demonstration strings, not real credentials. This local exchange does
not provide a distributed authorisation protocol, durable registry, rollback or
upstream Prompt API.
