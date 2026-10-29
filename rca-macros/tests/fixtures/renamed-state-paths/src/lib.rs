//! Checks generic state paths through a renamed RCA dependency.

use renamed_rca::{DependencyKey as _, KeyedState, StatePaths};

#[derive(StatePaths)]
pub struct Exported<T> {
    pub value: T,
}

#[derive(StatePaths)]
pub enum Aggregate<T> {
    Tuple(T, String),
    Named { value: T, label: String },
    Idle,
}

pub fn aggregate_paths_compile() {
    use_paths();
    assert_eq!(RenamedDependencyKey::live_value(), 7);
    let source = String::from("local");
    let path = AggregatePaths::<&str>::tuple();
    let mut state = path.embed((source.as_str(), String::from("before")));
    let (value, label) = path.get_mut(&mut state).unwrap();
    *value = "changed";
    label.push_str(" after");
    let (value, label) = path.get(&state).unwrap();
    assert_eq!((*value, label.as_str()), ("changed", "before after"));
    let named = AggregatePaths::<&str>::named();
    let _ = named.get(&named.embed((source.as_str(), String::new())));
    assert!(
        AggregatePaths::<&str>::idle()
            .get(&Aggregate::Idle)
            .is_some()
    );
}

struct ChildState;
struct DetailState;

#[derive(renamed_rca::DependencyKey)]
#[dependency(value = usize, live = 7)]
struct RenamedDependencyKey;

#[derive(StatePaths)]
struct AppState<Id, Row>
where
    Id: Eq + std::hash::Hash,
{
    child: ChildState,
    #[rca(optional)]
    detail: Option<DetailState>,
    #[rca(keyed)]
    rows: KeyedState<Id, Row>,
}

fn paths_compile<Id, Row>()
where
    Id: Eq + std::hash::Hash,
{
    let _ = AppStatePaths::<Id, Row>::child();
    let _ = AppStatePaths::<Id, Row>::detail();
    let _ = AppStatePaths::<Id, Row>::rows();
}

fn use_paths() {
    paths_compile::<u8, ()>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_paths_preserve_aggregate_and_keyed_state() {
        aggregate_paths_compile();
        let named = AggregatePaths::<&str>::named();
        let mut named_state = named.embed(("named", String::from("payload")));
        let (value, label) = named.get(&named_state).unwrap();
        assert_eq!((*value, label.as_str()), ("named", "payload"));
        let (value, label) = named.get_mut(&mut named_state).unwrap();
        *value = "changed";
        label.push_str(" updated");
        match &named_state {
            Aggregate::Named { value, label } => {
                assert_eq!((*value, label.as_str()), ("changed", "payload updated"));
            }
            _ => panic!("named projection changed the enum case"),
        }

        let idle = AggregatePaths::<&str>::idle();
        let mut idle_state = idle.embed(());
        assert!(matches!(idle_state, Aggregate::Idle));
        assert_eq!(idle.get_mut(&mut idle_state), Some(()));
        assert!(
            AggregatePaths::<&str>::tuple()
                .get_mut(&mut idle_state)
                .is_none()
        );
        assert!(named.get(&idle_state).is_none());
        assert!(named.get_mut(&mut idle_state).is_none());
        assert!(matches!(idle_state, Aggregate::Idle));
        assert!(idle.get(&named_state).is_none());
        assert!(idle.get_mut(&mut named_state).is_none());
        match &named_state {
            Aggregate::Named { value, label } => {
                assert_eq!((*value, label.as_str()), ("changed", "payload updated"));
            }
            _ => panic!("mismatching projection changed the enum case"),
        }
        assert!(
            AggregatePaths::<&str>::tuple()
                .get(&Aggregate::Idle)
                .is_none()
        );

        let mut exported = Exported { value: 3_u8 };
        *ExportedPaths::value().get_mut(&mut exported) = 9;
        assert_eq!(*ExportedPaths::value().get(&exported), 9);

        let mut state = AppState {
            child: ChildState,
            detail: None,
            rows: KeyedState::<u8, u8>::new(),
        };
        let detail = AppStatePaths::<u8, u8>::detail();
        assert!(detail.get(&state).is_none());
        state.detail = Some(DetailState);
        assert!(detail.get_mut(&mut state).is_some());

        let rows = AppStatePaths::<u8, u8>::rows();
        rows.get_mut(&mut state).insert(7, 3);
        assert_eq!(rows.get(&state).get(&7), Some(&3));
        *rows.get_mut(&mut state).get_mut(&7).unwrap() = 9;
        assert_eq!(state.rows.get(&7), Some(&9));
    }
}
