// Checks single-field state cases and explicitly skipped cases without Clone.

use rca_macros::StatePaths;

struct NonCloneNonDebug;

impl Default for NonCloneNonDebug {
    fn default() -> Self {
        Self
    }
}

#[derive(StatePaths)]
enum State<Row>
where
    Row: Default,
{
    Tuple(Row),
    Named {
        child: NonCloneNonDebug,
    },
    #[rca(skip)]
    Unit,
    #[rca(skip)]
    Multiple(NonCloneNonDebug, NonCloneNonDebug),
}

fn main() {
    let _ = StatePaths::<NonCloneNonDebug>::tuple();
    let _ = StatePaths::<NonCloneNonDebug>::child();
}
