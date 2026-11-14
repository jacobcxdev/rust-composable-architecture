# Correlated authorisation without a request registry

The [runnable example](../examples/authorisation.rs) presents an optional detail,
emits a typed authorisation request to its parent, and moves an owned reply back
into that same detail incarnation. Run it with:

```console
cargo run -p rca-example-composition-search --example authorisation
cargo test -p rca-example-composition-search --example authorisation
```

The request owns a Tokio one-shot sender. The sender, not its business ID, is the
correlation capability. The reply is non-Clone data; it does not grant permission
to change State. After awaiting it, the requester calls `update` through the
`TaskContext` it captured before suspension. RCA validates that exact incarnation
at the actor. The recipe never looks up a replacement by ID to apply an old reply.

## Two small application bounds

The application allows one active authorisation attempt. It uses a semaphore's
`try_acquire_owned`: a concurrent attempt returns `Busy` immediately. The permit
lives in the requester future through delivery, reply wait and scoped update.
Completion or actual future destruction releases it. This is application policy,
not an additional RCA admission guarantee.

The parent has a separate capacity-one inbox. Its synchronous Output handler
uses `try_send`; it never blocks or awaits during a transaction. A full inbox
answers `Busy`; a closed inbox continues the owned output, which becomes
`NoRecipient`. The two bounds are distinct: a cancelled request can leave an old
endpoint queued after its requester permit has been released. Arbitrary external
retention of already-received reply handles is not bounded by this recipe.

The fixed named-operation key uses `Coexist`. A second attempt must not replace
the first merely because it is going to fail the permit check.

## Outcomes and ownership

| Event | Result |
| --- | --- |
| No root recipient or closed respondent inbox | `NoRecipient` |
| Active-attempt limit or full respondent inbox | `Busy` |
| Recipient drops an accepted endpoint without replying | `Abandoned` |
| Reply-wait deadline expires | `TimedOut` |
| Output admission or scoped update fails | Original access reason and progress |
| Managed request is cancelled or the Store shuts down | Waiting task is dropped; ordinary completion callback need not run |

The response timeout starts **after Output delivery succeeds**. It is not a total
deadline for actor admission, response and State update. Admission can suspend
while retaining the permit. A ready reply can win even with a zero-duration
timeout; the timeout witness establishes that its receiver is actually Pending.

The task boundary handles an ordinary helper result through an application-owned
report callback. It does not attempt another update through a stale context, and
the demo never prints the credential.

`Reply::respond` takes its sender at most once. A duplicate attempt returns its
own still-owned payload. Sending after the receiver has been destroyed likewise
returns the original payload. A successful send transfers ownership: cancellation
immediately afterwards does not promise to recover a value already sent. Likewise,
an actor update error does not promise recovery after callback handoff.

## Cancellation, replacement and shutdown

Capture the operation wait while the requester is live, then request cancellation
and await that saved wait. Cancellation acknowledgement is not proof that the
future has been destroyed. The same distinction applies to Store shutdown.

A new detail may reuse the business ID with a different replacement identity.
Its request has a different one-shot receiver. An old endpoint cannot fulfil it.
Even an externally retained requester context, whose future is not automatically
cancelled with the managed task, cannot update that new incarnation after its old
reply arrives.

This is a local composition recipe, not a distributed authorisation protocol,
durable request registry, rollback facility or upstream Prompt API. Credentials
in the executable example are demonstration strings, not secrets.
