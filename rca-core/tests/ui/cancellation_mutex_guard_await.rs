use std::sync::Mutex;

use rca_core::Effect;

fn main() {
    let _effect = Effect::<()>::run(|_sender| async move {
        let value = Mutex::new(0);
        let mut guard = value.lock().unwrap();
        std::future::pending::<()>().await;
        *guard += 1;
    });
}
