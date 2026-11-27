//! The flags file of a Mac of the build farm, in shape (`farm-mac-flags` beside this file): what
//! the agent of a black-box test starts on.

/// The farm's Mac flags as committed beside this file.
const SHAPE: &str = include_str!("farm-mac-flags");

/// The place of the lease server's address in [`SHAPE`].
const SERVER: &str = "{server}";

/// The flags file of a Mac of the farm's shape whose lease server is at `server`.
pub fn flags(server: &str) -> String {
    assert_eq!(
        SHAPE.matches(SERVER).count(),
        1,
        "farm-mac-flags names {SERVER} once"
    );
    SHAPE.replace(SERVER, server)
}
