//! Process isolation for regressions that could otherwise hang the test runner.

/// Run one exact test in a bounded child, returning true in the parent.
pub fn run_in_subprocess(test: &str) -> bool {
    #[cfg(miri)]
    {
        // Miri cannot spawn a process. Use the same body and an external timeout.
        let _ = test;
        false
    }
    #[cfg(not(miri))]
    {
        use std::ffi::OsStr;
        use std::process::Command;
        use std::time::{Duration, Instant};

        const CHILD_TEST: &str = "RCA_EVENT_LISTENER_CHILD_TEST";
        assert_eq!(
            std::thread::current().name(),
            Some(test),
            "subprocess filter must match this test's name"
        );
        if std::env::var_os(CHILD_TEST).as_deref() == Some(OsStr::new(test)) {
            return false;
        }
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env(CHILD_TEST, test)
            .spawn()
            .expect("spawn isolated regression");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    assert!(
                        status.success(),
                        "isolated regression {test} failed: {status}"
                    );
                    return true;
                }
                Ok(None) => {}
                Err(error) => {
                    // Child does not stop itself when its handle is dropped.
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("poll isolated regression {test}: {error}");
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated regression {test} exceeded 30 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
