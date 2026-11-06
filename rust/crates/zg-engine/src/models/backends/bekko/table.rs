use tokio_util::sync::CancellationToken;

use crate::models::spi::ModelError;

pub(super) struct Table {
    pub(super) weights: Vec<u8>,
    pub(super) scales: Vec<f32>,
    pub(super) dimension: usize,
}

impl Table {
    pub(super) fn embed(
        &self,
        ids: &[u32],
        signal: Option<&CancellationToken>,
    ) -> Result<Vec<f32>, ModelError> {
        let mut vector = vec![0.0_f32; self.dimension];
        for (index, &id) in ids.iter().enumerate() {
            if index % 256 == 0 {
                check_cancelled(signal)?;
            }
            let row = usize::try_from(id).map_err(|_| invalid_token(id))?;
            let scale = *self.scales.get(row).ok_or_else(|| invalid_token(id))?;
            let offset = row * self.dimension;
            for (sum, &weight) in vector
                .iter_mut()
                .zip(&self.weights[offset..offset + self.dimension])
            {
                // Match ONNX Cast -> Mul -> ReduceSum. Never replace these with
                // mul_add or FP64 accumulation: both change the rounding.
                let scaled = f32::from(i8::from_ne_bytes([weight])) * scale;
                *sum += scaled;
            }
        }
        let count = u32::try_from(ids.len().max(1))
            .map_err(|_| ModelError::invalid_argument("Bekko token count is too large"))?;
        #[allow(clippy::cast_precision_loss)]
        let divisor = count as f32;
        for value in &mut vector {
            *value /= divisor;
        }
        // Same postprocessing as the ONNX backend: FP32 mean, FP64 norm,
        // then FP32 output. Unknown token ID 0 is deliberately retained.
        let squared_norm = vector
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum::<f64>();
        if squared_norm > 0.0 {
            let inverse = squared_norm.sqrt().recip();
            for value in &mut vector {
                #[allow(clippy::cast_possible_truncation)]
                {
                    *value = (f64::from(*value) * inverse) as f32;
                }
            }
        }
        check_cancelled(signal)?;
        Ok(vector)
    }
}

fn invalid_token(id: u32) -> ModelError {
    ModelError::invalid_argument(format!("Bekko embedding token ID out of range: {id}"))
}

pub(super) fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), ModelError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(ModelError::cancelled("Bekko embedding was cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "table/tests.rs"]
mod tests;
