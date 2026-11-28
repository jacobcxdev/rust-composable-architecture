use rca_core::local::Operation;

fn send(_: impl Send) {}

fn handle(operation: Operation) {
    send(operation);
}
fn status(operation: &Operation) {
    send(operation.status());
}
fn capture(operation: &Operation) {
    send(operation.capture());
}
fn cancel(operation: &Operation) {
    send(operation.cancel());
}

fn main() {}
