//! Fixed-code-budget PQ training/encoding using Microsoft's quantization crate.
#![allow(
    clippy::chunks_exact_to_as_chunks,
    reason = "preserve the byte-decoding loop used for the verified PQ artifacts"
)]

use diskann_quantization::{
    CompressInto, Parallelism,
    cancel::DontCancel,
    product::{
        tables::TransposedTable,
        train::{LightPQTrainingParameters, TrainQuantizer},
    },
    random::StdRngBuilder,
    views::ChunkOffsets,
};
use diskann_utils::views::rowmajor::{self, Matrix};
use rayon::prelude::*;
use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    time::Instant,
};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    assert_eq!(
        a.len(),
        7,
        "train|encode input output dimension chunks iterations|pivots"
    );
    let dim: usize = a[4].parse().unwrap();
    let chunks: usize = a[5].parse().unwrap();
    assert!(chunks > 0 && dim.is_multiple_of(chunks));
    let offsets = ChunkOffsets::new(
        (0..=chunks)
            .map(|i| i * (dim / chunks))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    )
    .unwrap();
    let started = Instant::now();
    let file = File::open(&a[2]).unwrap();
    let n = file.metadata().unwrap().len() as usize / (dim * 4);
    assert_eq!(file.metadata().unwrap().len() as usize, n * dim * 4);
    let mut reader = BufReader::new(file);
    let mut output = BufWriter::new(File::create(&a[3]).unwrap());
    if a[1] == "train" {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        let values: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| half::f16::from_f32(f32::from_le_bytes(b.try_into().unwrap())).to_f32())
            .collect();
        let data = rowmajor::Ref::try_from_data(&values, n, dim).unwrap();
        eprintln!("training rows={n} dim={dim} chunks={chunks}");
        let table = LightPQTrainingParameters::new(256, a[6].parse().unwrap())
            .train(
                data,
                offsets.as_view(),
                Parallelism::Rayon,
                &StdRngBuilder::new(20261104),
                &DontCancel,
            )
            .unwrap();
        for value in table.view_pivots().as_slice() {
            output.write_all(&value.to_le_bytes()).unwrap();
        }
    } else {
        assert_eq!(a[1], "encode");
        let bytes = std::fs::read(&a[6]).unwrap();
        assert_eq!(bytes.len(), 256 * dim * 4);
        let pivots: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        let table = TransposedTable::from_parts(
            rowmajor::Ref::try_from_data(&pivots, 256, dim).unwrap(),
            offsets,
        )
        .unwrap();
        output
            .write_all(zg_diskann::experimental_pq::MAGIC)
            .unwrap();
        output.write_all(&(dim as u32).to_le_bytes()).unwrap();
        output.write_all(&(chunks as u32).to_le_bytes()).unwrap();
        output.write_all(&(n as u64).to_le_bytes()).unwrap();
        output.write_all(&bytes).unwrap();
        for start in (0..n).step_by(4096) {
            let rows = (n - start).min(4096);
            let mut bytes = vec![0_u8; rows * dim * 4];
            reader.read_exact(&mut bytes).unwrap();
            let values: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|b| half::f16::from_f32(f32::from_le_bytes(b.try_into().unwrap())).to_f32())
                .collect();
            let mut codes = vec![0_u8; rows * chunks];
            codes
                .par_chunks_mut(chunks)
                .zip(values.par_chunks(dim))
                .for_each(|(code, vector)| table.compress_into(vector, code).unwrap());
            output.write_all(&codes).unwrap();
            if start % (4096 * 16) == 0 {
                eprintln!("encoded {} / {n}", start + rows);
            }
        }
    }
    output.flush().unwrap();
    eprintln!(
        "{} complete in {:.2}s",
        a[1],
        started.elapsed().as_secs_f64()
    );
}
