mod config;
pub mod bootstrap_config;

use std::fmt::{Display, Formatter};
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;
use futures::{StreamExt, TryStreamExt};
use log::{error, info};
use polars::error::PolarsError;
use polars::prelude::IntoLazy;
use tempfile::TempPath;
use tokio::runtime::Runtime;

use common::types::config::Config;
use data_harvester::step1_fetch_data::{fetch_dataset, FetchError};
use data_harvester::step2_import_data::{import_data, ImportError, ImportStepExtra};
use data_harvester::step3_validate_data::{validate_data, ValidateError, ValidateStepOutput};
use data_harvester::step4_merge_data::{merge, MergeError};
use data_harvester::step5_simplify::{simplify, SimplifyError};
use routing::algorithm::{PreprocessInit, PreprocessingError, PreprocessingInput};
use common::util::logging::{initialize_logging, run_with_spinner};
use common::util::speed::Speed;
use routing::stp::ScalableTransferPatternsAlgorithm;
use bootstrap_config::BootstrapConfig;
use crate::config::load_config;

type ALGORITHM = ScalableTransferPatternsAlgorithm;

// The maximum speed in km/h that any vehicle can travel
// This must be high enough, otherwise wrong routes might be calculated
pub const MAX_SPEED: Speed = Speed(500.0);

fn run() -> Result<(), DrinoError> {
    let bootstrap_config = BootstrapConfig::read();
    
    initialize_logging(bootstrap_config.clone().log_level.into());
    info!(target: "main", "Using temporary folder at {}", std::env::temp_dir().to_str().unwrap());
    
    let config = load_config(bootstrap_config)?;

    let result: Result<(), DrinoError> = match config {
        Config::Version1 { datasets, .. } => {
            let mut files_to_clean_up: Vec<PathBuf> = vec![];

            info!(target: "preprocessing", "Starting preprocessing");
            let preprocessing_start_time = SystemTime::now();

            let preprocessing_input = run_with_spinner("preprocessing", "Fetching and importing datasets", || {
                let rt = Runtime::new().unwrap();
                rt.block_on(async {
                    // TODO: Process all datasets
                    let datasets = datasets.into_iter().take(1);

                    let results = futures::stream::iter(datasets)
                        .then(|dataset| async move {
                            let fetch_out = fetch_dataset(dataset).await?;
                            let import_out = import_data(fetch_out).await?;
                            let validated = validate_data(import_out).await?;
                            Ok::<ValidateStepOutput, DrinoError>(validated)
                        })
                        .inspect_err(|err| {
                            error!("{}", err);
                        })
                        .collect::<Vec<Result<ValidateStepOutput, DrinoError>>>()
                        .await.into_iter()
                        .collect::<Result<Vec<ValidateStepOutput>, DrinoError>>()?;

                    results.iter().for_each(|result| {
                        match &result.extra {
                            ImportStepExtra::Gtfs { temporary_files, .. } => {
                                temporary_files.iter().for_each(|f| files_to_clean_up.push(f.clone()))
                            }
                        }
                    });

                    let merged = merge(results).await?;
                    let simplified = simplify(merged).await?;

                    Ok::<PreprocessingInput, DrinoError>(simplified)
                })
            })?;

            // TODO: Merge datasets (with deduplication) and frequency reduce calender times

            // Cache important (and small) tables like stops to speed up computation
            let cached_input = run_with_spinner("preprocessing", "Reading and caching timetable data", move || {
                Ok::<PreprocessingInput, DrinoError>(PreprocessingInput {
                    stops: preprocessing_input.stops.collect()?.lazy(),
                    stop_times: preprocessing_input.stop_times.collect()?.lazy(),
                    ..preprocessing_input
                })
            })?;

            let preprocessing_result = ALGORITHM::preprocess(cached_input)?;

            let elapsed = indicatif::HumanDuration(preprocessing_start_time.elapsed().unwrap());
            info!(target: "preprocessing", "Preprocessing finished in {}", elapsed);
            files_to_clean_up.into_iter()
                .for_each(|file| {
                    TempPath::from_path(file).close()
                        .expect("Unable to clean up temp files. Please clean up manually.");
                });
            Ok(())
        }
    };

    result
}

fn main() {    
    let _ = run()
        .map_err(|err| match err {
            DrinoError::ConfigFile(_) => {
                error!("Error while reading config file: {}", err);
            }
            _ => {
                error!("{}", err);
            }
        });
}

#[derive(thiserror::Error, Debug)]
pub enum DrinoError {
    ConfigFile(#[from] io::Error),
    Fetch(#[from] FetchError),
    Import(#[from] ImportError),
    Validate(#[from] ValidateError),
    Merge(#[from] MergeError),
    Simplify(#[from] SimplifyError),
    Polars(#[from] PolarsError),
    Preprocessing(#[from] PreprocessingError),
}

impl Display for DrinoError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let err: &dyn Display = match self {
            DrinoError::ConfigFile(err) => err,
            DrinoError::Fetch(err) => err,
            DrinoError::Import(err) => err,
            DrinoError::Validate(err) => err,
            DrinoError::Merge(err) => err,
            DrinoError::Simplify(err) => err,
            DrinoError::Polars(err) => err,
            DrinoError::Preprocessing(err) => err
        };
        write!(f, "{}", err)
    }
}