use std::{io::SeekFrom, path::Path};

use half::f16;
use serde_json::Value;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
};

use crate::models::spi::ModelError;

#[derive(Clone, Debug)]
pub(super) struct StaticEmbeddingTable {
    pub(super) values: Vec<f32>,
    pub(super) dimension: usize,
    pub(super) rows: usize,
}

#[derive(Clone, Copy)]
enum TensorDtype {
    F16,
    F32,
}

struct TensorLocation {
    absolute_start: usize,
    byte_length: usize,
    dtype: TensorDtype,
    dimension: usize,
    rows: usize,
}

pub(super) async fn load_static_embedding_table(
    path: &Path,
    tensor_name: &str,
    expected_dimension: usize,
) -> Result<StaticEmbeddingTable, ModelError> {
    let mut file = File::open(path).await.map_err(|error| {
        ModelError::storage_failure(format!("Unable to open Safetensors file: {error}"))
    })?;
    let file_size = file
        .metadata()
        .await
        .map_err(|error| {
            ModelError::storage_failure(format!("Unable to stat Safetensors file: {error}"))
        })?
        .len();
    if file_size < 9 {
        return Err(ModelError::storage_failure("Safetensors file is too small"));
    }

    let mut prefix = [0_u8; 8];
    file.read_exact(&mut prefix).await.map_err(|error| {
        ModelError::storage_failure(format!("Unable to read Safetensors header: {error}"))
    })?;
    let header_length = usize::try_from(u64::from_le_bytes(prefix))
        .map_err(|_| ModelError::storage_failure("Safetensors header length is invalid"))?;
    let data_start = 8_usize
        .checked_add(header_length)
        .ok_or_else(|| ModelError::storage_failure("Safetensors header length is invalid"))?;
    let data_start_u64 = u64::try_from(data_start)
        .map_err(|_| ModelError::storage_failure("Safetensors header length is invalid"))?;
    if data_start_u64 > file_size {
        return Err(ModelError::storage_failure(
            "Safetensors header length is invalid",
        ));
    }

    let mut header_bytes = vec![0_u8; header_length];
    file.read_exact(&mut header_bytes).await.map_err(|error| {
        ModelError::storage_failure(format!("Unable to read Safetensors header: {error}"))
    })?;
    let header: Value = serde_json::from_slice(&header_bytes).map_err(|error| {
        ModelError::storage_failure("Safetensors header is invalid JSON").with_cause(error)
    })?;
    let location = tensor_location(
        &header,
        tensor_name,
        expected_dimension,
        data_start,
        file_size,
    )?;

    file.seek(SeekFrom::Start(
        u64::try_from(location.absolute_start).map_err(|_| invalid_offsets(tensor_name))?,
    ))
    .await
    .map_err(|error| {
        ModelError::storage_failure(format!("Unable to seek Safetensors file: {error}"))
    })?;
    let mut data = vec![0_u8; location.byte_length];
    file.read_exact(&mut data).await.map_err(|error| {
        ModelError::storage_failure(format!("Safetensors file ended unexpectedly: {error}"))
    })?;

    let values = match location.dtype {
        TensorDtype::F16 => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| f16::from_bits(u16::from_le_bytes(*chunk)).to_f32())
            .collect(),
        TensorDtype::F32 => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect(),
    };
    Ok(StaticEmbeddingTable {
        values,
        dimension: location.dimension,
        rows: location.rows,
    })
}

fn tensor_location(
    header: &Value,
    tensor_name: &str,
    expected_dimension: usize,
    data_start: usize,
    file_size: u64,
) -> Result<TensorLocation, ModelError> {
    let tensor = header
        .get(tensor_name)
        .and_then(Value::as_object)
        .ok_or_else(|| incompatible_tensor(tensor_name))?;
    let dtype = tensor
        .get("dtype")
        .and_then(Value::as_str)
        .ok_or_else(|| incompatible_tensor(tensor_name))?;
    let shape = tensor
        .get("shape")
        .and_then(Value::as_array)
        .ok_or_else(|| incompatible_tensor(tensor_name))?;
    let offsets = tensor
        .get("data_offsets")
        .and_then(Value::as_array)
        .ok_or_else(|| incompatible_tensor(tensor_name))?;
    if shape.len() != 2 || offsets.len() != 2 {
        return Err(incompatible_tensor(tensor_name));
    }
    let dtype = match dtype {
        "F16" => TensorDtype::F16,
        "F32" => TensorDtype::F32,
        _ => return Err(incompatible_tensor(tensor_name)),
    };
    let rows = json_usize(&shape[0]).ok_or_else(|| incompatible_tensor(tensor_name))?;
    let dimension = json_usize(&shape[1]).ok_or_else(|| incompatible_tensor(tensor_name))?;
    if dimension != expected_dimension {
        return Err(incompatible_tensor(tensor_name));
    }
    let relative_start = json_usize(&offsets[0]).ok_or_else(|| incompatible_tensor(tensor_name))?;
    let relative_end = json_usize(&offsets[1]).ok_or_else(|| incompatible_tensor(tensor_name))?;
    let value_count = rows
        .checked_mul(dimension)
        .ok_or_else(|| invalid_offsets(tensor_name))?;
    let bytes_per_value = match dtype {
        TensorDtype::F16 => 2,
        TensorDtype::F32 => 4,
    };
    let tensor_byte_length = value_count
        .checked_mul(bytes_per_value)
        .ok_or_else(|| invalid_offsets(tensor_name))?;
    if relative_end < relative_start || relative_end - relative_start != tensor_byte_length {
        return Err(invalid_offsets(tensor_name));
    }
    let absolute_start = data_start
        .checked_add(relative_start)
        .ok_or_else(|| invalid_offsets(tensor_name))?;
    let absolute_end = data_start
        .checked_add(relative_end)
        .ok_or_else(|| invalid_offsets(tensor_name))?;
    let absolute_end_u64 = u64::try_from(absolute_end).map_err(|_| invalid_offsets(tensor_name))?;
    if absolute_end_u64 > file_size {
        return Err(invalid_offsets(tensor_name));
    }

    Ok(TensorLocation {
        absolute_start,
        byte_length: tensor_byte_length,
        dtype,
        dimension,
        rows,
    })
}

fn json_usize(value: &Value) -> Option<usize> {
    value.as_u64().and_then(|value| usize::try_from(value).ok())
}

fn incompatible_tensor(name: &str) -> ModelError {
    ModelError::storage_failure(format!(
        "Safetensors tensor '{name}' is missing or incompatible"
    ))
}

fn invalid_offsets(name: &str) -> ModelError {
    ModelError::storage_failure(format!("Safetensors tensor '{name}' has invalid offsets"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use half::f16;
    use tempfile::TempDir;

    use super::load_static_embedding_table;

    #[tokio::test]
    async fn parses_typescript_compatible_f32_and_f16_tables() {
        let root = TempDir::new().expect("temporary directory should be created");
        for (name, dtype, data) in [
            (
                "f32",
                "F32",
                [1.0_f32, 2.0, 3.0, 4.0]
                    .into_iter()
                    .flat_map(f32::to_le_bytes)
                    .collect::<Vec<_>>(),
            ),
            (
                "f16",
                "F16",
                [1.0_f32, 2.0, 3.0, 4.0]
                    .into_iter()
                    .flat_map(|value| f16::from_f32(value).to_bits().to_le_bytes())
                    .collect::<Vec<_>>(),
            ),
        ] {
            let path = root.path().join(format!("{name}.safetensors"));
            write_fixture(&path, dtype, &data).await;
            let table = load_static_embedding_table(&path, "embeddings", 2)
                .await
                .expect("fixture table should parse");
            assert_eq!(table.rows, 2);
            assert_eq!(table.dimension, 2);
            assert_eq!(table.values, [1.0, 2.0, 3.0, 4.0]);
        }
    }

    async fn write_fixture(path: &Path, dtype: &str, data: &[u8]) {
        let mut header = format!(
            r#"{{"embeddings":{{"dtype":"{dtype}","shape":[2,2],"data_offsets":[0,{}]}}}}"#,
            data.len()
        );
        while header.len() % 8 != 0 {
            header.push(' ');
        }
        let mut bytes = u64::try_from(header.len())
            .expect("fixture header should fit u64")
            .to_le_bytes()
            .to_vec();
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(data);
        tokio::fs::write(path, bytes)
            .await
            .expect("fixture should be written");
    }
}
