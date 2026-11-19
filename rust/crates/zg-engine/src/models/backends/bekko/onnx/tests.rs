use super::*;

fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    while value >= 128 {
        bytes.push(u8::try_from(value & 127).expect("7 bits") | 128);
        value >>= 7;
    }
    bytes.push(u8::try_from(value).expect("7 bits"));
    bytes
}

fn number(tag: u64, value: u64) -> Vec<u8> {
    [varint(tag << 3), varint(value)].concat()
}

fn bytes(tag: u64, value: &[u8]) -> Vec<u8> {
    [
        varint((tag << 3) | 2),
        varint(value.len().try_into().expect("size")),
        value.to_vec(),
    ]
    .concat()
}

fn tensor(name: &str, dtype: u64, dims: &[u64], raw: &[u8]) -> Vec<u8> {
    let shape = dims.iter().flat_map(|&d| varint(d)).collect::<Vec<_>>();
    bytes(
        5,
        &[
            bytes(1, &shape),
            number(2, dtype),
            bytes(8, name.as_bytes()),
            bytes(9, raw),
        ]
        .concat(),
    )
}

pub(in crate::models::backends::bekko) fn fixture() -> Vec<u8> {
    let mut graph = Vec::new();
    for node in NODES {
        let mut data = Vec::new();
        for input in node.inputs {
            data.extend(bytes(1, input.as_bytes()));
        }
        data.extend(bytes(2, node.output.as_bytes()));
        data.extend(bytes(4, node.op.as_bytes()));
        if let Some((name, value)) = node.attribute {
            data.extend(bytes(
                5,
                &[bytes(1, name.as_bytes()), number(3, value), number(20, 2)].concat(),
            ));
        }
        graph.extend(bytes(1, &data));
    }
    graph.extend(tensor("embedding_int8", 3, &[3, 2], &[3, 4, 0, 2, 255, 0]));
    let scales = [2_f32, 0.5, 1.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    graph.extend(tensor("row_scales", 1, &[3, 1], &scales));
    graph.extend(tensor("sequence_axis", 7, &[1], &1_i64.to_le_bytes()));
    graph.extend(tensor("last_axis", 7, &[1], &2_i64.to_le_bytes()));
    graph.extend(tensor("one", 1, &[], &1_f32.to_le_bytes()));
    for input in ["input_ids", "attention_mask"] {
        graph.extend(bytes(11, &bytes(1, input.as_bytes())));
    }
    graph.extend(bytes(12, &bytes(1, b"sentence_embedding")));
    bytes(7, &graph)
}

#[test]
fn loads_signed_weights_and_per_row_scales() {
    let table = load(&fixture(), 3, 2, None).expect("Bekko graph");
    assert_eq!(table.weights, [3, 4, 0, 2, 255, 0]);
    assert_eq!(table.scales, [2.0, 0.5, 1.0]);
    assert_eq!(table.embed(&[2], None).expect("signed row"), [-1.0, 0.0]);
}

#[test]
fn rejects_every_truncated_prefix_wrong_shapes_and_changed_graph() {
    let fixture = fixture();
    for end in 0..fixture.len() {
        assert!(load(&fixture[..end], 3, 2, None).is_err(), "prefix {end}");
    }
    assert!(load(&fixture, 4, 2, None).is_err());
    assert!(load(&fixture, 3, 3, None).is_err());
    let mut changed = fixture.clone();
    let position = changed
        .windows(6)
        .position(|w| w == b"Gather")
        .expect("operator");
    changed[position] = b'X';
    assert!(load(&changed, 3, 2, None).is_err());
    let duplicate = [fixture.clone(), fixture].concat();
    assert!(load(&duplicate, 3, 2, None).is_err());
}

#[test]
fn rejects_nonfinite_scale_wrong_axes_and_cancelled_load() {
    let fixture = fixture();
    let scales = [2_f32, 0.5, 1.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    let offset = fixture
        .windows(scales.len())
        .position(|w| w == scales)
        .expect("scales");
    for value in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        let mut changed = fixture.clone();
        changed[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(load(&changed, 3, 2, None).is_err());
    }
    let axis = tensor("sequence_axis", 7, &[1], &1_i64.to_le_bytes());
    let offset = fixture
        .windows(axis.len())
        .position(|w| w == axis)
        .expect("axis");
    let mut changed = fixture.clone();
    changed[offset + axis.len() - 8] = 0;
    assert!(load(&changed, 3, 2, None).is_err());
    let signal = CancellationToken::new();
    signal.cancel();
    assert_eq!(
        load(&fixture, 3, 2, Some(&signal))
            .err()
            .expect("cancelled")
            .code(),
        crate::EngineError::CANCELLED
    );
}
