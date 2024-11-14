use crate::algorithm::{PreprocessInit, PreprocessingError, PreprocessingInput, PreprocessingResult};
use crate::direct_connections::DirectConnections;
use crate::raptor::{RaptorAlgorithm, TripAtStopTimeMap, TripsByLineAndStopMap};
use crate::transfers::crow_fly::CrowFlyTransferProvider;
use chrono::{DateTime, Utc};
use common::types::{LineId, SeqNum, StopId, TripId};
use hashbrown::{HashMap, HashSet};
use itertools::{izip, Itertools};
use polars::error::PolarsError;
use polars::prelude::*;
use std::ops::{BitAnd, BitOr};

impl RaptorAlgorithm {
    pub fn preprocess(
        PreprocessingInput { stops, .. }: PreprocessingInput,
        DirectConnections { expanded_lines, line_progressions, .. }: DirectConnections,
    ) -> PreprocessingResult<RaptorAlgorithm> {
        let stops_vec: Vec<StopId> = stops.clone()
            .select(&[col("stop_id")])
            .collect()?.column("stop_id")?
            .u32()?.to_vec()
            .into_iter().filter_map(|x| x.map(StopId))
            .collect();

        let (stops_by_line, lines_by_stops) = {
            let mut stops_by_line = HashMap::new();
            let mut lines_by_stops = HashMap::new();

            let [line_ids, stop_ids, sequence_numbers] = line_progressions.get_columns()
            else { return Err(PreprocessingError::Polars(PolarsError::ColumnNotFound("".into()))); };

            let [line_ids, stop_ids, sequence_numbers] =
                [line_ids.u32()?, stop_ids.u32()?, sequence_numbers.u32()?];

            for (line_id, stop_id, seq_num) in izip!(line_ids, stop_ids, sequence_numbers) {
                let line_id = line_id.unwrap().into();
                let stop_id = stop_id.unwrap().into();
                let seq_num = seq_num.unwrap().into();
                
                stops_by_line.entry(line_id).or_insert(vec![])
                    .push(stop_id);

                lines_by_stops.entry(stop_id).or_insert(HashSet::new())
                    .insert((line_id, seq_num));
            }

            Ok::<(HashMap<LineId, Vec<StopId>>, HashMap<StopId, HashSet<(LineId, SeqNum)>>), PreprocessingError>
                ((stops_by_line, lines_by_stops))
        }?;
        debug_assert!(stops_vec.len() == lines_by_stops.len());


        let lines = expanded_lines.clone()
            .select(["line_id", "stop_id", "stop_sequence", "trip_id", "arrival_time", "departure_time"])?;

        let (arrivals, departures) = {
            let sorted_lines = lines.clone().sort(
                ["line_id", "trip_id", "stop_sequence"],
                SortMultipleOptions::default()
                    .with_maintain_order(false)
                    .with_order_descending(false),
            )?;
            let [_line_ids, stop_ids, _sequence_numbers, trip_ids, arrival_times, departure_times] =
                sorted_lines.get_columns()
            else { return Err(PreprocessingError::Polars(PolarsError::ColumnNotFound("".into()))); };

            let [stop_ids, trip_ids] =
                [stop_ids.u32()?, trip_ids.u32()?];
            let arrival_times = arrival_times.duration()?;
            let departure_times = departure_times.duration()?;

            let mut arrivals = HashMap::new();
            let mut departures = HashMap::new();
            for (trip_id, stop_id, arrival_time, departure_time) in izip!(trip_ids, stop_ids, arrival_times.iter(), departure_times.iter()) {
                let trip_id = TripId(trip_id.unwrap());
                let stop_id = StopId(stop_id.unwrap());
                let arrival_time = arrival_time.unwrap();
                let departure_time = departure_time.unwrap();
                
                // TODO: Fix date time handling
                let arrival_time = DateTime::from_timestamp_millis(arrival_time).unwrap();
                let departure_time = DateTime::from_timestamp_millis(departure_time).unwrap();
                
                arrivals.insert((trip_id, stop_id), arrival_time);
                departures.insert((trip_id, stop_id), departure_time);
            }
            
            Ok::<(TripAtStopTimeMap, TripAtStopTimeMap), PreprocessingError>
                ((arrivals, departures))
        }?;


        let trips_by_line_and_stop_df = lines.clone().lazy()
            .sort(["departure_time"], SortMultipleOptions::default().with_maintain_order(false))
            .group_by(&[col("line_id"), col("stop_id")])
            .agg(&[col("trip_id"), col("departure_time")])
            .collect()?;
        let [line_ids, stop_ids, trips_ids, departures_times] = trips_by_line_and_stop_df.get_columns()
        else { return Err(PreprocessingError::Polars(PolarsError::ColumnNotFound("".into()))); };
        let line_ids = line_ids.u32()?;
        let stop_ids = stop_ids.u32()?;
        let trips_ids = trips_ids.list()?;
        let departures_times = departures_times.list()?;

        let mut trips_by_line_and_stop: TripsByLineAndStopMap = HashMap::new();

        for (line_id, stop_id, trips, departures) in izip!(line_ids, stop_ids, trips_ids, departures_times) {
            let trips = trips.unwrap();
            let departures = departures.unwrap();
            let departures_trips = departures.duration()?.iter().zip(trips.u32()?)
                .filter_map(|(departure, trip)| {
                    departure.map(|departure| {
                        // TODO: Fix date conversion
                        (DateTime::from_timestamp_millis(departure).unwrap(), TripId(trip.unwrap()))
                    })
                });

            trips_by_line_and_stop.insert(
                (LineId(line_id.unwrap()), StopId(stop_id.unwrap())),
                departures_trips.collect(),
            );
        }

        if cfg!(debug_assertions) {
            // Assert monotonous increase in departure time within a trip
            for ((line, _), departures) in trips_by_line_and_stop.iter() {
                // We can only check increase for trips that have at least two stops
                if departures.len() >= 2 {
                    let (last_departure_time, last_trip) = departures.first().unwrap();
                    for (departure_time, trip) in departures.iter().skip(1) {
                        debug_assert!(
                            departure_time >= last_departure_time,
                            "Expected departure time ({departure_time}) to not be smaller than previous ({last_departure_time}) in trips_by_line_and_stop. Offending Trip: {trip:?} (compared to {last_trip:?}) on line {line:?}. Excerpt from lines DF:\n{}\nExcerpt from trips_by_line_and_stop:\n{:#?}",
                            lines.clone().filter(
                                &lines.column("line_id")?.as_materialized_series().equal(line.0)?.bitand(
                                    lines.column("trip_id")?.as_materialized_series().equal(trip.0)?.bitor(
                                        lines.column("trip_id")?.as_materialized_series().equal(last_trip.0)?
                                    )
                                )
                            )?,
                            trips_by_line_and_stop.iter()
                                .filter(|((l, _), _)| l == line)
                                .collect_vec()
                        );
                    }
                }
            }
        }

        Ok(Self {
            stops: stops_vec,
            stops_by_line,
            lines_by_stops,
            arrivals,
            departures,
            trips_by_line_and_stop,
            transfer_provider: Box::new(CrowFlyTransferProvider::from_stops(stops)?),
        })
    }
}

impl PreprocessInit for RaptorAlgorithm {
    fn preprocess(input: PreprocessingInput) -> PreprocessingResult<RaptorAlgorithm> {
        let direct_connections = DirectConnections::try_from(input.clone())?;
        Self::preprocess(input, direct_connections)
    }
}


#[cfg(test)]
mod tests {
    use itertools::Itertools;
    use polars::df;
    use polars::frame::DataFrame;
    use polars::prelude::*;

    use super::*;

    #[test]
    fn test_preprocessing() {
        let departure_times: Series = [0; 15]
            .into_iter().collect::<Series>()
            .cast(&DataType::Duration(TimeUnit::Milliseconds)).unwrap();

        let preprocessing_in = PreprocessingInput {
            services: DataFrame::empty().lazy(),
            stops: df!(
                "stop_id" => &[0u32, 1, 2, 3, 4, 5],
                "lat"     => &[0.0f32, 1.0, 5.0, -10.0, 80.0, -42.0 ],
                "lon"     => &[0.0f32, 1.0, 5.0, -10.0, 80.0, -42.0 ],
            ).unwrap().lazy(),
            trips: df!(
                "trip_id" => &[0u32, 1, 2, 3],
            ).unwrap().lazy(),
            stop_times: df!(
                "trip_id"        => &[0u32, 0, 0,  1,  0,  1, 1, 1,  2, 2,  3, 3, 3, 3, 3],
                "stop_id"        => &[0u32, 1, 2,  2,  3,  3, 4, 5,  3, 4,  0, 1, 2, 3, 4],
                "arrival_time"   => departure_times.clone(),
                "departure_time" => departure_times.clone(),
                "stop_sequence"  => &[0u32, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 15]
            ).unwrap().lazy(),
        };

        let preprocessing_out = <RaptorAlgorithm as PreprocessInit>::preprocess(preprocessing_in, None).unwrap();

        assert!(list_eq(&preprocessing_out.stops, &vec![0u32, 1, 2, 3, 4, 5].into_iter().map(|x| StopId(x)).collect()));
        // TODO: Test all of preprocessing_out
    }

    fn list_eq<T>(a: &Vec<T>, b: &Vec<T>) -> bool
    where
        T: PartialEq + Ord,
    {
        a.iter().sorted().collect_vec();
        b.iter().sorted().collect_vec();

        a == b
    }

    // TODO: More test cases. This one test passed, despite the function being wrong!
}