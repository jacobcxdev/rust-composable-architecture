//! Writes one counter value after loading and closing its persistent document.
//!
//! Invalid arguments fail before file access. Success requires a persistence
//! receipt, backend quiescence and Store shutdown, in that order.
//! A reporting error does not roll back an acknowledged save.

use std::error::Error;
use std::io::{self, Write as _};

use rca_core::Store;
use rca_example_persistent_counter::codec::CounterCodec;
use rca_persistence::{Action, Document, FileBackend, FileOptions, SyncMode};
use rca_tokio::TokioRuntime;

/// Parses native paths and reports success only after the final save completes.
fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or("usage: rca-example-persistent-counter PATH VALUE")?;
    let value = args
        .next()
        .ok_or("missing value")?
        .into_string()
        .map_err(|_invalid_value| "value is not UTF-8")?
        .parse::<u64>()?;
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    let sync = if cfg!(unix) {
        SyncMode::FileAndDirectory
    } else {
        SyncMode::File
    };
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let backend = FileBackend::new(
        path,
        // The explicit codec emits exactly 16 bytes, with no trailing data.
        FileOptions {
            maximum_bytes: 16,
            sync,
        },
        host.handle().clone(),
    )?;
    host.block_on(async {
        let (state, document) = Document::new(0, backend.clone(), CounterCodec);
        let store = Store::builder(state, document, TokioRuntime::current()?).build()?;
        let result = async {
            store.send(Action::load()).await?.finish().await;
            if let Some(failure) = store.read(|s| s.failure().cloned()).await? {
                store.send(Action::abort()).await?.finish().await;
                return Err(Box::<dyn Error>::from(failure));
            }
            let old = store.read(|s| *s.value()).await?;
            // Dropping this causal handle does not cancel persistence.
            let _ = store.send(Action::set(value)).await?;
            let (close, receipt) = Action::close();
            let _ = store.send(close).await?;
            // A causal task finishing is not persistence success: await the receipt.
            receipt.await?;
            Ok(old)
        }
        .await;
        // Even a failed or abandoned save must be quiescent before restart.
        backend.close().await;
        let state = store.shutdown_into_state().await?;
        let old = result?;
        let mut output = io::stdout().lock();
        writeln!(output, "{old} -> {}", state.value())?;
        output.flush()?;
        Ok(())
    })
}
