use super::*;

#[test]
fn rejects_truncated_overflowing_and_invalid_wire_data() {
    for data in [
        vec![0],
        vec![0x0b],
        vec![0x08, 0x80],
        vec![0x12, 4, 1],
        vec![0x09, 1],
        vec![0x0d, 1],
        vec![0xff; 11],
        [vec![0x08], vec![0xff; 9], vec![2]].concat(),
    ] {
        assert!(Reader::new(&data).next().is_err(), "{data:?}");
    }
}

#[test]
fn decodes_maximum_varint_and_rejects_wrong_field_type() {
    let data = [vec![0x08], vec![0xff; 9], vec![1]].concat();
    let mut reader = Reader::new(&data);
    assert_eq!(
        reader
            .next()
            .expect("field")
            .expect("integer")
            .1
            .integer()
            .expect("integer"),
        u64::MAX
    );
    assert!(reader.next().expect("end").is_none());
    assert!(Value::Integer(1).bytes().is_err());
    assert!(Value::Bytes(&[]).integer().is_err());
    assert!(Value::Bytes(&[255]).string().is_err());
}
