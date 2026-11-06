//! Extract only the exact row-wise INT8 lookup/mean graph we implement.
//! Artifacts are hash-verified by the shared downloader before reaching here.

use std::collections::HashMap;

use tokio_util::sync::CancellationToken;

use crate::models::spi::ModelError;

use super::{
    protobuf::{Reader, Value, invalid},
    table::{Table, check_cancelled},
};

struct Node {
    op: &'static str,
    inputs: &'static [&'static str],
    output: &'static str,
    attribute: Option<(&'static str, u64)>,
}

const NODES: &[Node] = &[
    Node {
        op: "Gather",
        inputs: &["embedding_int8", "input_ids"],
        output: "selected_int8",
        attribute: Some(("axis", 0)),
    },
    Node {
        op: "Gather",
        inputs: &["row_scales", "input_ids"],
        output: "selected_scales",
        attribute: Some(("axis", 0)),
    },
    Node {
        op: "Cast",
        inputs: &["selected_int8"],
        output: "selected_float",
        attribute: Some(("to", 1)),
    },
    Node {
        op: "Mul",
        inputs: &["selected_float", "selected_scales"],
        output: "token_embeddings",
        attribute: None,
    },
    Node {
        op: "Cast",
        inputs: &["attention_mask"],
        output: "float_mask",
        attribute: Some(("to", 1)),
    },
    Node {
        op: "Unsqueeze",
        inputs: &["float_mask", "last_axis"],
        output: "expanded_mask",
        attribute: None,
    },
    Node {
        op: "Mul",
        inputs: &["token_embeddings", "expanded_mask"],
        output: "masked_embeddings",
        attribute: None,
    },
    Node {
        op: "ReduceSum",
        inputs: &["masked_embeddings", "sequence_axis"],
        output: "sum_embeddings",
        attribute: Some(("keepdims", 0)),
    },
    Node {
        op: "ReduceSum",
        inputs: &["float_mask", "sequence_axis"],
        output: "token_count",
        attribute: Some(("keepdims", 1)),
    },
    Node {
        op: "Max",
        inputs: &["token_count", "one"],
        output: "safe_count",
        attribute: None,
    },
    Node {
        op: "Div",
        inputs: &["sum_embeddings", "safe_count"],
        output: "sentence_embedding",
        attribute: None,
    },
];

pub(super) fn load(
    bytes: &[u8],
    rows: usize,
    dimension: usize,
    signal: Option<&CancellationToken>,
) -> Result<Table, ModelError> {
    check_cancelled(signal)?;
    let mut reader = Reader::new(bytes);
    let mut graph = None;
    while let Some((tag, value)) = reader.next()? {
        if tag == 7 && graph.replace(value.bytes()?).is_some() {
            return Err(invalid());
        }
    }
    let tensors = graph_tensors(graph.ok_or_else(invalid)?)?;
    if tensors.len() != 5 || rows == 0 || dimension == 0 {
        return Err(invalid());
    }
    let weights = tensor_data(&tensors, "embedding_int8", 3, &[rows, dimension])?;
    if weights.len() != rows.checked_mul(dimension).ok_or_else(invalid)? {
        return Err(invalid());
    }
    let raw_scales = tensor_data(&tensors, "row_scales", 1, &[rows, 1])?;
    if raw_scales.len() != rows.checked_mul(4).ok_or_else(invalid)?
        || tensor_data(&tensors, "sequence_axis", 7, &[1])? != 1_i64.to_le_bytes()
        || tensor_data(&tensors, "last_axis", 7, &[1])? != 2_i64.to_le_bytes()
        || tensor_data(&tensors, "one", 1, &[])? != 1_f32.to_le_bytes()
    {
        return Err(invalid());
    }
    let mut scales = Vec::with_capacity(rows);
    for (index, chunk) in raw_scales.as_chunks::<4>().0.iter().enumerate() {
        if index % 1024 == 0 {
            check_cancelled(signal)?;
        }
        let scale = f32::from_le_bytes(*chunk);
        if !scale.is_finite() || scale <= 0.0 {
            return Err(invalid());
        }
        scales.push(scale);
    }
    let weights = weights.to_vec();
    check_cancelled(signal)?;
    Ok(Table {
        weights,
        scales,
        dimension,
    })
}

fn graph_tensors(bytes: &[u8]) -> Result<HashMap<&str, Tensor<'_>>, ModelError> {
    let mut reader = Reader::new(bytes);
    let mut tensors = HashMap::new();
    let mut node_index = 0;
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    while let Some((tag, value)) = reader.next()? {
        match tag {
            1 => {
                validate_node(value.bytes()?, NODES.get(node_index).ok_or_else(invalid)?)?;
                node_index += 1;
            }
            5 => {
                let tensor = Tensor::parse(value.bytes()?)?;
                if tensors.insert(tensor.name, tensor).is_some() {
                    return Err(invalid());
                }
            }
            11 => inputs.push(value_name(value.bytes()?)?),
            12 => outputs.push(value_name(value.bytes()?)?),
            15 => return Err(invalid()), // Sparse initializers are unsupported.
            _ => {}
        }
    }
    if node_index != NODES.len()
        || inputs != ["input_ids", "attention_mask"]
        || outputs != ["sentence_embedding"]
    {
        return Err(invalid());
    }
    Ok(tensors)
}

fn value_name(bytes: &[u8]) -> Result<&str, ModelError> {
    let mut reader = Reader::new(bytes);
    let mut name = None;
    while let Some((tag, value)) = reader.next()? {
        if tag == 1 && name.replace(value.string()?).is_some() {
            return Err(invalid());
        }
    }
    name.ok_or_else(invalid)
}

fn validate_node(bytes: &[u8], expected: &Node) -> Result<(), ModelError> {
    let mut reader = Reader::new(bytes);
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut op = None;
    let mut attributes = Vec::new();
    while let Some((tag, value)) = reader.next()? {
        match tag {
            1 => inputs.push(value.string()?),
            2 => outputs.push(value.string()?),
            4 => {
                if op.replace(value.string()?).is_some() {
                    return Err(invalid());
                }
            }
            5 => attributes.push(integer_attribute(value.bytes()?)?),
            7 | 8 if !value.string()?.is_empty() => return Err(invalid()),
            _ => {}
        }
    }
    if op != Some(expected.op)
        || inputs != expected.inputs
        || outputs != [expected.output]
        || attributes != expected.attribute.into_iter().collect::<Vec<_>>()
    {
        return Err(invalid());
    }
    Ok(())
}

fn integer_attribute(bytes: &[u8]) -> Result<(&str, u64), ModelError> {
    let mut reader = Reader::new(bytes);
    let (mut name, mut integer, mut kind) = (None, None, None);
    while let Some((tag, value)) = reader.next()? {
        let duplicate = match tag {
            1 => name.replace(value.string()?).is_some(),
            3 => integer.replace(value.integer()?).is_some(),
            20 => kind.replace(value.integer()?).is_some(),
            13 => false, // doc_string
            _ => return Err(invalid()),
        };
        if duplicate {
            return Err(invalid());
        }
    }
    if kind != Some(2) {
        return Err(invalid());
    }
    Ok((name.ok_or_else(invalid)?, integer.ok_or_else(invalid)?))
}

struct Tensor<'a> {
    name: &'a str,
    dtype: u64,
    dims: Vec<usize>,
    raw: &'a [u8],
}

impl<'a> Tensor<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, ModelError> {
        let mut reader = Reader::new(bytes);
        let (mut name, mut dtype, mut raw) = (None, None, None);
        let mut dims = Vec::new();
        while let Some((tag, value)) = reader.next()? {
            let duplicate = match tag {
                1 => {
                    match value {
                        Value::Integer(value) => {
                            dims.push(usize::try_from(value).map_err(|_| invalid())?);
                        }
                        Value::Bytes(bytes) => {
                            let mut packed = Reader::new(bytes);
                            while !packed.is_empty() {
                                dims.push(
                                    usize::try_from(packed.varint()?).map_err(|_| invalid())?,
                                );
                            }
                        }
                        Value::Fixed => return Err(invalid()),
                    }
                    false
                }
                2 => dtype.replace(value.integer()?).is_some(),
                8 => name.replace(value.string()?).is_some(),
                9 => raw.replace(value.bytes()?).is_some(),
                12 | 16 => false,           // doc_string, metadata_props
                _ => return Err(invalid()), // No external/typed/segmented tensors.
            };
            if duplicate || dims.len() > 2 {
                return Err(invalid());
            }
        }
        Ok(Self {
            name: name.ok_or_else(invalid)?,
            dtype: dtype.ok_or_else(invalid)?,
            dims,
            raw: raw.ok_or_else(invalid)?,
        })
    }
}

fn tensor_data<'a>(
    tensors: &HashMap<&str, Tensor<'a>>,
    name: &str,
    dtype: u64,
    dims: &[usize],
) -> Result<&'a [u8], ModelError> {
    let tensor = tensors.get(name).ok_or_else(invalid)?;
    if tensor.dtype != dtype || tensor.dims != dims {
        return Err(invalid());
    }
    Ok(tensor.raw)
}

#[cfg(test)]
#[path = "onnx/tests.rs"]
pub(super) mod tests;
