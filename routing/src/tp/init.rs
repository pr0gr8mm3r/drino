use crate::algorithm::{AllRange, PreprocessInit, PreprocessingInput, PreprocessingResult, Range};
use crate::direct_connections::DirectConnections;
use crate::raptor::RaptorAlgorithm;
use crate::tp::transfer_patterns::TransferPatterns;
use crate::tp::TransferPatternsAlgorithm;
use async_trait::async_trait;
use chrono::{DateTime, Duration};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use std::sync::{Arc, Mutex};

// TODO: Experiment with this value to see which one is useful
const CHUNK_SIZE: u64 = 5;

#[async_trait]
impl PreprocessInit for TransferPatternsAlgorithm {
    fn preprocess(input: PreprocessingInput, progress_bars: Option<&MultiProgress>) -> PreprocessingResult<Self> {
        let direct_connections = DirectConnections::try_from(input.clone())?;
        let raptor = Arc::new(RaptorAlgorithm::preprocess(input, direct_connections.clone())?);
        let transfer_patterns = Arc::new(Mutex::new(TransferPatterns::new()?));
        
        let pb = progress_bars.map(|pbs| {
            pbs.add(
                ProgressBar::new(raptor.stops.len() as u64)
                    .with_message("Progressing stops in cluster...")
                    .with_style(
                        ProgressStyle::with_template("[{elapsed}] {msg} [{wide_bar}] {human_pos}/{human_len}")
                            .unwrap().progress_chars("=> ")
                    )
            )
        });

        raptor.stops.par_iter()
            // Process in chunks, so that inserting into transfer patterns data structure is more
            // efficient (less waiting for Mutexes etc.)
            .chunks(CHUNK_SIZE as usize)
            .for_each(|stops| {
                
                let raptor = Arc::clone(&raptor);
                let transfer_patterns = Arc::clone(&transfer_patterns);

                let results = stops.into_iter()
                    .map(|stop| {
                        raptor.query_range_all(
                            Range {
                                earliest_departure: DateTime::from_timestamp_millis(0).unwrap(),
                                start: *stop,
                                range: Duration::weeks(1),
                            }
                        )
                    })
                    .filter_map(|result| {
                        match result {
                            Ok(res) => { Some(res) }
                            Err(_) => { None }
                        }
                    })
                    .collect();

                // Add this chunk to our existing transfer patterns
                let mut transfer_patterns = transfer_patterns.lock().unwrap();
                transfer_patterns.add_multiple(results).unwrap();

                pb.clone().map(|pb| pb.inc(CHUNK_SIZE));
            });
        
        pb.map(|pb| { pb.finish_with_message("All stops in cluster finished") });

        let transfer_patterns = Arc::try_unwrap(transfer_patterns)
            .expect("Lock is still owned by others").into_inner().unwrap();


        Ok(Self {
            direct_connections,
            transfer_patterns,
        })
    }
}