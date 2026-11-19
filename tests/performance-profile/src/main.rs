fn main() {
    match dispatch(std::env::args().skip(1)) {
        Ok(record) => println!("{record}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

include!("profile_items.rs");
