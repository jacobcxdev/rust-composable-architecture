use rca_core::KeyedState;

fn main() {
    let selected;
    {
        let states: KeyedState<_, _> = [(1, String::from("one"))].into_iter().collect();
        selected = states.try_filter(|_, _| Ok::<_, ()>(true)).unwrap();
    }
    println!("{}", selected[0].1);
}
