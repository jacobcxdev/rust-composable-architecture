macro_rules! malformed_state {
    () => {
        #[derive(rca_macros::StatePaths)]
        struct $crate {
            value: u8,
        }
    };
}

macro_rules! malformed_action {
    () => {
        #[derive(rca_macros::ActionPaths)]
        enum $crate {
            Value(u8),
        }
    };
}

mod state_case {
    malformed_state!();
}

mod action_case {
    malformed_action!();
}

macro_rules! malformed_descriptor {
    () => {
        #[derive(rca_macros::CaseDescriptors)]
        enum $crate {
            Value(u8),
        }
    };
}

mod descriptor_case {
    malformed_descriptor!();
}

fn main() {}
