#![expect(
    non_upper_case_globals,
    reason = "the const parameter deliberately shares the generated namespace name"
)]

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Packet<const PacketPaths: usize> {
    Bytes([u8; PacketPaths]),
}

fn main() {
    let packet = Packet::<3>::Bytes([1, 2, 3]);
    assert_eq!(PacketPaths::<3>::bytes().extract(&packet), Some(&[1, 2, 3]));
}
