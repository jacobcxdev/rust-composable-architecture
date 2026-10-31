/// Cumulative allocation totals and currently live allocations at one boundary.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Snapshot {
    pub blocks: u64,
    pub bytes: u64,
    pub live_blocks: usize,
    pub live_bytes: usize,
}

impl Snapshot {
    fn now() -> Self {
        #[cfg(feature = "allocations")]
        {
            let stats = dhat::HeapStats::get();
            Self {
                blocks: stats.total_blocks,
                bytes: stats.total_bytes,
                live_blocks: stats.curr_blocks,
                live_bytes: stats.curr_bytes,
            }
        }
        #[cfg(not(feature = "allocations"))]
        Self::default()
    }

    fn json(self) -> String {
        format!(
            "{{\"allocated_blocks\":{},\"allocated_bytes\":{},\"live_blocks\":{},\"live_bytes\":{}}}",
            self.blocks, self.bytes, self.live_blocks, self.live_bytes
        )
    }
}

/// Records construction, work, checking and teardown in the caller's fixed order.
pub(crate) struct Measurement {
    snapshots: [Snapshot; 5],
    intermediate: Option<(Boundary, Snapshot)>,
    #[cfg(feature = "allocations")]
    cache: Option<ExecutorCache>,
    #[cfg(feature = "allocations")]
    profiler: HeapProfile,
}

#[cfg(feature = "allocations")]
/// Finalises the profiler in memory, including on early return or unwind.
struct HeapProfile {
    profiler: Option<std::mem::ManuallyDrop<dhat::Profiler>>,
}

#[cfg(feature = "allocations")]
impl HeapProfile {
    fn new() -> Self {
        Self {
            profiler: Some(std::mem::ManuallyDrop::new(
                dhat::Profiler::builder().trim_backtraces(None).build(),
            )),
        }
    }

    fn finish(&mut self) -> Option<String> {
        // Take ownership before finalising: even an unwind cannot invoke the
        // ordinary file-writing destructor or finalise the session twice.
        self.profiler
            .take()
            .map(|mut profiler| profiler.drop_and_get_memory_output())
    }
}

#[cfg(feature = "allocations")]
impl Drop for HeapProfile {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

impl Measurement {
    pub(crate) fn new(cache: Option<ExecutorCache>) -> Self {
        #[cfg(not(feature = "allocations"))]
        let _ = cache;
        #[cfg(feature = "allocations")]
        let profiler = HeapProfile::new();
        Self {
            snapshots: [Snapshot::now(); 5],
            intermediate: None,
            #[cfg(feature = "allocations")]
            cache,
            #[cfg(feature = "allocations")]
            profiler,
        }
    }

    pub(crate) fn construction_finished(&mut self) {
        self.snapshots[1] = Snapshot::now();
    }

    pub(crate) fn work_finished(&mut self) {
        self.snapshots[2] = Snapshot::now();
    }

    pub(crate) fn warmup_finished(&mut self) {
        self.intermediate = Some((Boundary::Warmup, Snapshot::now()));
    }

    pub(crate) fn cold_finished(&mut self) {
        self.intermediate = Some((Boundary::Cold, Snapshot::now()));
    }

    pub(crate) fn validation_finished(&mut self) {
        self.snapshots[3] = Snapshot::now();
    }

    #[cfg_attr(
        not(feature = "allocations"),
        expect(
            clippy::unnecessary_wraps,
            reason = "allocation-enabled builds validate retention through the same interface"
        )
    )]
    pub(crate) fn finish(mut self) -> Result<Option<Record>, &'static str> {
        self.snapshots[4] = Snapshot::now();
        #[cfg(feature = "allocations")]
        {
            let stats = dhat::HeapStats::get();
            let record = Record {
                snapshots: self.snapshots,
                intermediate: self.intermediate,
                cache: self.cache,
                peak_blocks: stats.max_blocks,
                peak_bytes: stats.max_bytes,
            };
            let profile = self
                .profiler
                .finish()
                .expect("measurement owns the session");
            let [initial, _, _, _, retained] = record.snapshots;
            let blocks = retained
                .live_blocks
                .checked_sub(initial.live_blocks)
                .ok_or("live allocation counts decreased below baseline")?;
            let bytes = retained
                .live_bytes
                .checked_sub(initial.live_bytes)
                .ok_or("live allocation bytes decreased below baseline")?;
            verify_live_stacks(&profile, self.cache, blocks, bytes)?;
            // Read configuration and write only after the session has stopped
            // and retention passed, keeping export work outside all snapshots.
            // Each process needs its own absolute path; outputs are never replaced.
            if let Some(path) = std::env::var_os("RCA_DHAT_PROFILE") {
                export_profile(std::path::Path::new(&path), &profile)?;
            }
            Ok(Some(record))
        }
        #[cfg(not(feature = "allocations"))]
        Ok(None)
    }
}

/// Checked phase snapshots and whole-run peaks, ready for formatting after profiling.
pub(crate) struct Record {
    snapshots: [Snapshot; 5],
    intermediate: Option<(Boundary, Snapshot)>,
    cache: Option<ExecutorCache>,
    peak_blocks: usize,
    peak_bytes: usize,
}

impl Record {
    pub(crate) fn json(&self) -> String {
        let [initial, construction, work, validation, teardown] = self.snapshots;
        let residual_blocks = teardown.live_blocks - initial.live_blocks;
        let residual_bytes = teardown.live_bytes - initial.live_bytes;
        let cache = self.cache.map_or("none", ExecutorCache::name);
        if let Some((boundary, intermediate)) = self.intermediate {
            return format!(
                "{{\"instrument\":\"dhat-0.3.3\",\"snapshots\":[{},{},{},{},{},{}],\"phases\":{{\"construction\":{},\"{}\":{},\"work\":{},\"validation\":{},\"teardown\":{}}},\"whole_run_peak_blocks\":{},\"whole_run_peak_bytes\":{},\"executor_thread_local\":{{\"source\":\"{}\",\"retained_blocks\":{},\"retained_bytes\":{}}}}}",
                initial.json(),
                construction.json(),
                intermediate.json(),
                work.json(),
                validation.json(),
                teardown.json(),
                phase(initial, construction),
                match boundary {
                    Boundary::Cold => "cold",
                    Boundary::Warmup => "warmup",
                },
                phase(construction, intermediate),
                phase(intermediate, work),
                phase(work, validation),
                phase(validation, teardown),
                self.peak_blocks,
                self.peak_bytes,
                cache,
                residual_blocks,
                residual_bytes
            );
        }
        format!(
            "{{\"instrument\":\"dhat-0.3.3\",\"snapshots\":[{},{},{},{},{}],\"phases\":{{\"construction\":{},\"work\":{},\"validation\":{},\"teardown\":{}}},\"whole_run_peak_blocks\":{},\"whole_run_peak_bytes\":{},\"executor_thread_local\":{{\"source\":\"{}\",\"retained_blocks\":{},\"retained_bytes\":{}}}}}",
            initial.json(),
            construction.json(),
            work.json(),
            validation.json(),
            teardown.json(),
            phase(initial, construction),
            phase(construction, work),
            phase(work, validation),
            phase(validation, teardown),
            self.peak_blocks,
            self.peak_bytes,
            cache,
            residual_blocks,
            residual_bytes
        )
    }
}

#[derive(Clone, Copy)]
enum Boundary {
    Cold,
    Warmup,
}

/// Identifies the sole permitted retained allocation by size and owning stack.
#[derive(Clone, Copy)]
pub(crate) enum ExecutorCache {
    Native,
    Deterministic,
}

impl ExecutorCache {
    fn name(self) -> &'static str {
        match self {
            Self::Native => "tokio-1.53.3::runtime::park::CURRENT_PARKER",
            Self::Deterministic => "futures-executor-0.3.34::CURRENT_THREAD_NOTIFY",
        }
    }

    #[cfg(feature = "allocations")]
    fn bytes(self) -> usize {
        // The pinned executor sources and captured DHAT stacks establish these
        // Arc-owned TLS layouts. Size is platform-derived; any other residual
        // fails qualification instead of being silently attributed to a cache.
        let payload = match self {
            Self::Native => std::mem::size_of::<(
                std::sync::atomic::AtomicUsize,
                std::sync::Mutex<()>,
                std::sync::Condvar,
            )>(),
            Self::Deterministic => {
                std::mem::size_of::<(std::thread::Thread, std::sync::atomic::AtomicBool)>()
            }
        };
        payload + 2 * std::mem::size_of::<usize>()
    }

    #[cfg(feature = "allocations")]
    fn owns_stack(self, frames: &[&str]) -> bool {
        fn symbol(frame: &str) -> Option<&str> {
            frame
                .split_once(": ")
                .and_then(|(_, rest)| rest.split_once(" ("))
                .map(|(name, _)| name)
        }
        let (initializer, owner) = match self {
            Self::Native => (
                "tokio::runtime::park::CURRENT_PARKER::__rust_std_internal_init_fn",
                "<alloc::sync::Arc<tokio::runtime::park::Inner>>::new",
            ),
            Self::Deterministic => (
                "futures_executor::local_pool::CURRENT_THREAD_NOTIFY::__rust_std_internal_init_fn",
                "<alloc::sync::Arc<futures_executor::local_pool::ThreadNotify>>::new",
            ),
        };
        frames
            .iter()
            .any(|frame| symbol(frame) == Some(initializer))
            && frames.iter().any(|frame| symbol(frame) == Some(owner))
    }
}

fn phase(before: Snapshot, after: Snapshot) -> String {
    format!(
        "{{\"allocated_blocks\":{},\"allocated_bytes\":{},\"live_blocks_before\":{},\"live_bytes_before\":{},\"live_blocks_after\":{},\"live_bytes_after\":{}}}",
        after
            .blocks
            .checked_sub(before.blocks)
            .expect("allocation counts never decrease"),
        after
            .bytes
            .checked_sub(before.bytes)
            .expect("allocation bytes never decrease"),
        before.live_blocks,
        before.live_bytes,
        after.live_blocks,
        after.live_bytes
    )
}

#[cfg(feature = "allocations")]
fn verify_live_stacks(
    profile: &str,
    cache: Option<ExecutorCache>,
    blocks: usize,
    bytes: usize,
) -> Result<(), &'static str> {
    let profile: serde_json::Value =
        serde_json::from_str(profile).map_err(|_| "malformed DHAT profile")?;
    if profile["dhatFileVersion"] != 2 || profile["mode"] != "rust-heap" {
        return Err("unsupported DHAT profile");
    }
    let points = profile["pps"]
        .as_array()
        .ok_or("missing DHAT allocation records")?;
    let frames = profile["ftbl"]
        .as_array()
        .ok_or("missing DHAT frame table")?;
    if frames.iter().any(|frame| !frame.is_string()) {
        return Err("invalid DHAT frame table");
    }
    let mut observed_blocks = 0_usize;
    let mut observed_bytes = 0_usize;
    for point in points {
        let count = |field: &str| {
            point[field]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("invalid DHAT live count")
        };
        let live_blocks = count("ebk")?;
        let live_bytes = count("eb")?;
        let stack = point["fs"]
            .as_array()
            .ok_or("missing DHAT stack")?
            .iter()
            .map(|index| {
                index
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .and_then(|index| frames.get(index))
                    .and_then(serde_json::Value::as_str)
                    .ok_or("invalid DHAT frame index")
            })
            .collect::<Result<Vec<_>, _>>()?;
        observed_blocks = observed_blocks
            .checked_add(live_blocks)
            .ok_or("DHAT live count overflow")?;
        observed_bytes = observed_bytes
            .checked_add(live_bytes)
            .ok_or("DHAT live bytes overflow")?;
        if live_blocks != 0 || live_bytes != 0 {
            let cache = cache.ok_or("unexplained allocation retention after teardown")?;
            if live_blocks != 1 || live_bytes != cache.bytes() || !cache.owns_stack(&stack) {
                return Err("unowned allocation retention after teardown");
            }
        }
    }
    if observed_blocks != blocks || observed_bytes != bytes || observed_blocks > 1 {
        return Err("DHAT live records disagree with measured retention");
    }
    Ok(())
}

#[cfg(any(test, feature = "allocations"))]
/// Writes the original profile only to a new absolute path.
fn export_profile(path: &std::path::Path, profile: &str) -> Result<(), &'static str> {
    use std::io::Write;

    if !path.is_absolute() {
        return Err("DHAT profile output path must be absolute");
    }
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(profile.as_bytes())
    };
    write().map_err(|error| {
        eprintln!("DHAT profile export to {} failed: {error}", path.display());
        "DHAT profile export failed"
    })
}

#[cfg(test)]
/// Owns one exact fixture file and its otherwise empty temporary directory.
struct TestFile(std::path::PathBuf);

#[cfg(test)]
impl TestFile {
    fn new(name: &str, filename: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("rca-profile-test-{}-{name}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory.join(filename))
    }
}

#[cfg(test)]
impl Drop for TestFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot clean test profile: {error}"),
        }
        std::fs::remove_dir(self.0.parent().unwrap()).unwrap();
    }
}

#[cfg(test)]
mod profile_export_tests {
    use super::{TestFile, export_profile};

    const PROFILE: &str = r#"{"dhatFileVersion":2,"mode":"rust-heap","pps":[],"ftbl":["[root]"]}"#;

    #[test]
    fn writes_complete_profile_without_reserialising() {
        let output = TestFile::new("complete", "profile.json");
        export_profile(&output.0, PROFILE).unwrap();
        assert_eq!(std::fs::read(&output.0).unwrap(), PROFILE.as_bytes());
    }

    #[test]
    fn preserves_existing_output_and_reports_failure() {
        let output = TestFile::new("existing", "profile.json");
        std::fs::write(&output.0, b"caller-owned capture\n").unwrap();
        assert!(export_profile(&output.0, PROFILE).is_err());
        assert_eq!(std::fs::read(&output.0).unwrap(), b"caller-owned capture\n");
    }

    #[test]
    fn reports_io_failure_without_creating_parent_directories() {
        let output = TestFile::new("missing-parent", "profile.json");
        let parent = output.0.parent().unwrap().join("missing");
        assert!(export_profile(&parent.join("profile.json"), PROFILE).is_err());
        assert!(!parent.exists());
    }

    #[test]
    fn rejects_relative_and_empty_output_paths() {
        for path in ["profile.json", ""] {
            assert!(export_profile(std::path::Path::new(path), PROFILE).is_err());
        }
    }
}

#[cfg(all(test, feature = "allocations"))]
pub(crate) mod tests {
    use super::{ExecutorCache, Measurement, TestFile, verify_live_stacks};
    use std::hint::black_box;

    fn in_file_safe_child(name: &str) -> bool {
        const CHILD: &str = "RCA_PERFORMANCE_FILE_CHILD";
        const SENTINEL: &[u8] = b"caller-owned heap capture\n";
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return true;
        }
        let heap = TestFile::new(name.rsplit("::").next().unwrap(), "dhat-heap.json");
        std::fs::write(&heap.0, SENTINEL).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, name)
            .current_dir(heap.0.parent().unwrap());
        let output = crate::tests::allocation_child::output(&mut command);
        let contents = std::fs::read(&heap.0).unwrap();
        assert!(
            contents == SENTINEL,
            "early exit overwrote caller heap capture"
        );
        crate::tests::allocation_child::require_one_test(&output);
        false
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn file_safe_child_rejects_empty_selection() {
        let result = std::panic::catch_unwind(|| in_file_safe_child("missing::cleanup_test"));
        let panic = result.expect_err("empty cleanup selection must fail");
        let message = panic
            .downcast_ref::<String>()
            .expect("formatted diagnostic");
        assert!(message.starts_with("isolated cleanup test did not pass exactly once:"));
        let directory = std::env::temp_dir().join(format!(
            "rca-profile-test-{}-cleanup_test",
            std::process::id()
        ));
        assert!(
            !directory.exists(),
            "failed child left its fixture directory"
        );
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn early_error_preserves_caller_file_and_releases_session() {
        fn fail_before_finish() -> Result<(), &'static str> {
            let _measurement = Measurement::new(None);
            Err("expected early error")?;
            Ok(())
        }
        if !in_file_safe_child(
            "measurement::tests::early_error_preserves_caller_file_and_releases_session",
        ) {
            return;
        }
        assert_eq!(fail_before_finish(), Err("expected early error"));
        Measurement::new(None).finish().unwrap();
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn unwind_preserves_caller_file_and_releases_session() {
        if !in_file_safe_child(
            "measurement::tests::unwind_preserves_caller_file_and_releases_session",
        ) {
            return;
        }
        let result = std::panic::catch_unwind(|| {
            let _measurement = Measurement::new(None);
            panic!("expected unwind before finish");
        });
        let panic = result.expect_err("the measurement must unwind with its caller");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"expected unwind before finish")
        );
        Measurement::new(None).finish().unwrap();
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn attributes_known_allocations_and_drops() {
        if !crate::tests::in_child("measurement::tests::attributes_known_allocations_and_drops") {
            return;
        }
        let mut measurement = Measurement::new(None);
        let construction = black_box(Box::new([0_u8; 64]));
        measurement.construction_finished();
        let work = black_box(Box::new([0_u8; 128]));
        measurement.work_finished();
        measurement.validation_finished();
        drop(black_box(Box::new([0_u8; 256])));
        drop(construction);
        drop(work);
        let record = measurement.finish().unwrap().unwrap();
        let [a, b, c, _, d] = record.snapshots;
        assert_eq!(b.blocks - a.blocks, 1);
        assert_eq!(b.bytes - a.bytes, 64);
        assert_eq!(c.bytes - b.bytes, 128);
        assert_eq!(c.live_blocks - a.live_blocks, 2);
        assert_eq!(c.live_bytes - a.live_bytes, 192);
        assert_eq!(d.bytes - c.bytes, 256);
        assert_eq!(d.live_bytes, a.live_bytes);
        assert_eq!(d.live_blocks, a.live_blocks);
        assert_eq!(record.peak_bytes, 448);
        assert_eq!(record.peak_blocks, 3);
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn attributes_known_allocations_across_warmup_boundary() {
        if !crate::tests::in_child(
            "measurement::tests::attributes_known_allocations_across_warmup_boundary",
        ) {
            return;
        }
        let mut measurement = Measurement::new(None);
        let construction = black_box(Box::new([0_u8; 64]));
        measurement.construction_finished();
        let warmup = black_box(Box::new([0_u8; 128]));
        measurement.warmup_finished();
        let work = black_box(Box::new([0_u8; 256]));
        measurement.work_finished();
        drop(black_box(Box::new([0_u8; 512])));
        measurement.validation_finished();
        drop(black_box(Box::new([0_u8; 1024])));
        drop((construction, warmup, work));
        let record = measurement.finish().unwrap().unwrap();
        let json: serde_json::Value = serde_json::from_str(&record.json()).unwrap();
        assert_eq!(json["snapshots"].as_array().unwrap().len(), 6);
        for (phase, bytes, live) in [
            ("construction", 64, 64),
            ("warmup", 128, 192),
            ("work", 256, 448),
            ("validation", 512, 448),
            ("teardown", 1024, 0),
        ] {
            assert_eq!(json["phases"][phase]["allocated_blocks"], 1, "{phase}");
            assert_eq!(json["phases"][phase]["allocated_bytes"], bytes, "{phase}");
            assert_eq!(json["phases"][phase]["live_bytes_after"], live, "{phase}");
        }
        assert_eq!(json["whole_run_peak_bytes"], 1472);
        assert_eq!(json["executor_thread_local"]["retained_blocks"], 0);
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn attributes_known_allocations_across_cold_boundary() {
        if !crate::tests::in_child(
            "measurement::tests::attributes_known_allocations_across_cold_boundary",
        ) {
            return;
        }
        let mut measurement = Measurement::new(None);
        let construction = black_box(Box::new([0_u8; 64]));
        measurement.construction_finished();
        let cold = black_box(Box::new([0_u8; 128]));
        measurement.cold_finished();
        let work = black_box(Box::new([0_u8; 256]));
        measurement.work_finished();
        drop(black_box(Box::new([0_u8; 512])));
        measurement.validation_finished();
        drop(black_box(Box::new([0_u8; 1024])));
        drop((construction, cold, work));
        let record = measurement.finish().unwrap().unwrap();
        let json: serde_json::Value = serde_json::from_str(&record.json()).unwrap();
        assert!(json["phases"].get("warmup").is_none());
        assert_eq!(json["snapshots"].as_array().unwrap().len(), 6);
        for (phase, bytes, live, blocks) in [
            ("construction", 64, 64, 1),
            ("cold", 128, 192, 2),
            ("work", 256, 448, 3),
            ("validation", 512, 448, 3),
            ("teardown", 1024, 0, 0),
        ] {
            assert_eq!(json["phases"][phase]["allocated_blocks"], 1);
            assert_eq!(json["phases"][phase]["allocated_bytes"], bytes);
            assert_eq!(json["phases"][phase]["live_bytes_after"], live);
            assert_eq!(json["phases"][phase]["live_blocks_after"], blocks);
        }
        assert_eq!(json["whole_run_peak_bytes"], 1472);
        assert_eq!(json["executor_thread_local"]["retained_blocks"], 0);
        assert_eq!(json["executor_thread_local"]["retained_bytes"], 0);
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn rejects_unexplained_live_allocations() {
        if !crate::tests::in_child("measurement::tests::rejects_unexplained_live_allocations") {
            return;
        }
        let mut measurement = Measurement::new(None);
        let retained = black_box(Box::new([0_u8; 512]));
        measurement.construction_finished();
        measurement.work_finished();
        measurement.validation_finished();
        assert!(measurement.finish().is_err());
        drop(retained);
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn empty_measurement_does_not_allocate() {
        if !crate::tests::in_child("measurement::tests::empty_measurement_does_not_allocate") {
            return;
        }
        let mut measurement = Measurement::new(None);
        measurement.construction_finished();
        measurement.work_finished();
        measurement.validation_finished();
        let record = measurement.finish().unwrap().unwrap();
        for snapshot in record.snapshots {
            assert_eq!(snapshot.blocks, 0);
            assert_eq!(snapshot.bytes, 0);
            assert_eq!(snapshot.live_blocks, 0);
            assert_eq!(snapshot.live_bytes, 0);
        }
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn rejects_cache_sized_unrelated_owner_after_clean_session() {
        if !crate::tests::in_child(
            "measurement::tests::rejects_cache_sized_unrelated_owner_after_clean_session",
        ) {
            return;
        }
        let (name, cache) = if cfg!(feature = "deterministic") {
            ("store-deterministic", ExecutorCache::Deterministic)
        } else {
            ("store-native", ExecutorCache::Native)
        };
        let config =
            crate::workloads::Config::parse([name, "0", "1"].into_iter().map(str::to_owned))
                .unwrap();
        crate::run(config).unwrap();
        let mut measurement = Measurement::new(Some(cache));
        let unrelated = black_box(vec![0_u8; cache.bytes()].into_boxed_slice());
        measurement.construction_finished();
        measurement.work_finished();
        measurement.validation_finished();
        assert!(
            measurement.finish().is_err(),
            "cache-sized unrelated owner was labelled as executor TLS"
        );
        drop(unrelated);
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn rejects_cold_cache_sized_unrelated_owner() {
        if !crate::tests::in_child("measurement::tests::rejects_cold_cache_sized_unrelated_owner") {
            return;
        }
        let mut measurement = Measurement::new(Some(ExecutorCache::Native));
        let unrelated = black_box(vec![0_u8; ExecutorCache::Native.bytes()].into_boxed_slice());
        measurement.construction_finished();
        measurement.work_finished();
        measurement.validation_finished();
        assert!(
            measurement.finish().is_err(),
            "cold unrelated owner was labelled as executor TLS"
        );
        drop(unrelated);
    }

    #[test]
    fn attribution_rejects_malformed_and_unknown_live_records() {
        for profile in [
            "not-json",
            "{}",
            r#"{"dhatFileVersion":2,"mode":"rust-heap","pps":[],"ftbl":[]}"#,
            r#"{"dhatFileVersion":2,"mode":"rust-heap","pps":[{"eb":32,"ebk":1,"fs":[0]}],"ftbl":["0x1: unrelated::owner (owner.rs:1:1)"]}"#,
            r#"{"dhatFileVersion":2,"mode":"rust-heap","pps":[{"eb":32,"ebk":1,"fs":[99]}],"ftbl":["[root]"]}"#,
            r#"{"dhatFileVersion":2,"mode":"rust-heap","pps":[{"eb":32,"ebk":1}],"ftbl":["[root]"]}"#,
            r#"{"dhatFileVersion":2,"mode":"rust-heap","pps":[{"eb":-1,"ebk":1,"fs":[]}],"ftbl":[]}"#,
            r#"{"dhatFileVersion":2,"mode":"rust-heap","pps":[{"eb":32,"ebk":1,"fs":[0]}],"ftbl":[null]}"#,
        ] {
            assert!(
                verify_live_stacks(profile, Some(ExecutorCache::Deterministic), 1, 32).is_err(),
                "accepted malformed or unowned live allocation: {profile}"
            );
        }
    }
}
