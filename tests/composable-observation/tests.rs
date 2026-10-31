#[test]
fn paced_publication_and_unregistration() {
    assert_eq!(common::bounded(exercise, common::Mode::Native), Ok(()));
}

#[test]
fn omitted_repeated_publication_is_rejected() {
    assert_eq!(
        common::bounded(exercise, common::Mode::OmitRepeated),
        Err("missing publication")
    );
}

#[test]
fn previous_value_publication_is_rejected() {
    assert_eq!(
        common::bounded(exercise, common::Mode::PreviousValue),
        Err("wrong publication")
    );
}

#[test]
fn omitted_unregistration_is_rejected() {
    assert_eq!(
        common::bounded(exercise, common::Mode::KeepRegistered),
        Err("publication remained active")
    );
}

#[test]
fn lifecycle_snapshots_follow_successful_work() {
    fn observed(mode: common::Mode) -> Result<(), &'static str> {
        use common::Point;
        let mut points = Vec::new();
        exercise_observed(mode, &mut |point| points.push(point))?;
        if points
            != [
                Point::Construction,
                Point::Registration,
                Point::Initial,
                Point::Checked,
                Point::Unregistration,
            ]
        {
            return Err("wrong lifecycle snapshots");
        }
        Ok(())
    }
    assert_eq!(common::bounded(observed, common::Mode::Native), Ok(()));
}
