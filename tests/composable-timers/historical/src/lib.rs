//! Checks timer policies in the pinned historical Composable revision.

#[cfg(test)]
#[path = "../../mutation.rs"]
mod mutation;

#[cfg(test)]
#[path = "../../legacy.rs"]
mod tests;

#[cfg(test)]
#[path = "../../throttle_legacy.rs"]
mod throttle;

#[cfg(test)]
#[path = "../../leading_legacy.rs"]
mod leading;
