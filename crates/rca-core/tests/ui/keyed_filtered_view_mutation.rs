use rca_core::KeyedState;

fn main() {
    let mut states: KeyedState<_, _> = [(1, String::from("one"))].into_iter().collect();
    let selected = states.try_filter(|_, _| Ok::<_, ()>(true)).unwrap();
    states.try_insert_at(0, 2, String::from("two")).unwrap();
    println!("{}", selected[0].1);
}
