use rca_macros::Snapshot;
use std::rc::Rc;
type Borrowed = &'static str;
#[derive(Clone)]
struct CloneOnly;

#[derive(Snapshot)]
struct ImplicitBorrow {
    value: Borrowed,
}
#[derive(Snapshot)]
struct ImplicitHandle {
    value: Rc<usize>,
}
#[derive(Snapshot)]
struct UnsupportedNested {
    value: Vec<CloneOnly>,
}

fn wrong_signature(_: usize) -> usize {
    0
}
fn wrong_return(_: &usize) -> String {
    String::new()
}
#[derive(Snapshot)]
struct WrongSignature(#[snapshot(with = wrong_signature, value = usize)] usize);
#[derive(Snapshot)]
struct WrongReturn(#[snapshot(with = wrong_return, value = usize)] usize);

#[derive(Snapshot)]
struct BorrowedCustom(#[snapshot(with = wrong_return, value = &'static str)] usize);
#[derive(Snapshot)]
struct PointerCustom(#[snapshot(with = wrong_return, value = Option<*const usize>)] usize);
#[derive(Snapshot)]
struct FunctionCustom(#[snapshot(with = wrong_return, value = fn() -> usize)] usize);

struct CaptureDestination {
    value: String,
}
#[derive(Snapshot)]
#[snapshot(into = CaptureDestination)]
struct WrongDeclaredDestinationValue {
    #[snapshot(with = wrong_return, value = usize)]
    value: usize,
}

fn main() {}
