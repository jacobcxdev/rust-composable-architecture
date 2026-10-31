use std::sync::Mutex;

use rca_core::Effect;

fn main() {
    let _effect = Effect::<u8>::run(|sender| async move {
        let value = Mutex::new(0);
        let owned = {
            let mut guard = value.lock().unwrap();
            *guard += 1;
            *guard
        };
        std::future::ready(()).await;
        sender.send(owned).await.unwrap();
    });
}
