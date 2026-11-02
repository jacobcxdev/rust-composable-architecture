#![expect(
    non_upper_case_globals,
    reason = "the const parameter deliberately shares the generated namespace name"
)]

use rca_macros::StatePaths;

#[derive(StatePaths)]
struct Packet<const PacketPaths: usize> {
    bytes: [u8; PacketPaths],
}

fn main() {
    let packet = Packet::<3> { bytes: [1, 2, 3] };
    assert_eq!(PacketPaths::<3>::bytes().get(&packet), &[1, 2, 3]);
}
