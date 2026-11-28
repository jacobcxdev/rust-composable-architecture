//! Links a minimal RCA effect so release builds can measure binary size.

fn main() {
    std::hint::black_box(rca::Effect::<()>::none());
}
