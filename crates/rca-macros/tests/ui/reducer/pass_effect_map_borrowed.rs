use rca::Effect;

fn map_borrowed<'a>(effect: Effect<&'a usize>) -> Effect<&'a usize> {
    effect.map(|value| value)
}

fn select_local<'a>(_: &'a usize) -> Effect<&'a usize> {
    map_borrowed(Effect::none())
}

fn main() {
    let value = 7;
    drop(select_local(&value));
}
