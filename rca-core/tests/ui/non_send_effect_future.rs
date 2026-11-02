// A shared effect cannot retain Rc ownership across suspension.

use std::rc::Rc;

use rca_core::Effect;

fn main() {
    let _effect = Effect::<()>::run(|_sender| async move {
        let value = Rc::new(());
        std::future::pending::<()>().await;
        drop(value);
    });
}
