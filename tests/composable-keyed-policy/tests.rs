// Both engines must pass the native workload and reject each control for the
// intended reason. An unrelated send, shutdown or deadline error is not success.

#[test]
fn native_keyed_policy_trace() {
    assert_eq!(common::bounded(exercise, common::Mode::Native), Ok(()));
}

#[test]
fn shared_policy_is_rejected() {
    assert_eq!(
        common::bounded(exercise, common::Mode::SharedPolicy),
        Err("wrong keyed trace")
    );
}

#[test]
fn wrong_key_embedding_is_rejected() {
    assert_eq!(
        common::bounded(exercise, common::Mode::WrongKey),
        Err("wrong keyed trace")
    );
}

#[test]
fn retained_replacement_policy_is_rejected() {
    assert_eq!(
        common::bounded(exercise, common::Mode::RetainPolicy),
        Err("wrong final children")
    );
}
