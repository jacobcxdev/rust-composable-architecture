#[path = "../../legacy.rs"]
mod adapter;
#[path = "../../allocations.rs"]
mod allocations;

fn main() {
    match allocations::run("upstream", |observe| {
        adapter::exercise_observed(adapter::common::Mode::Native, observe)
    }) {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
