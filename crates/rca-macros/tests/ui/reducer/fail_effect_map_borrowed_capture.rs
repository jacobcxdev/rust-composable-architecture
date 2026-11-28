use rca::Effect;

fn main() {
    let captured = 7usize;
    let _ = Effect::send(1usize).map(|value| value + captured);
}
