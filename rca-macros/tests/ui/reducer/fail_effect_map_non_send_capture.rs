use rca::Effect;
use std::rc::Rc;

fn main() {
    let captured = Rc::new(7usize);
    let _ = Effect::send(1usize).map(move |value| value + *captured);
}
