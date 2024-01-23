//! See <https://www.dvrpc.org/traffic/> for additional information about traffic counting.
use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use csv::{Reader, ReaderBuilder};
use log::{error, info, LevelFilter};
use simplelog::{
    ColorChoice, CombinedLogger, ConfigBuilder, TermLogger, TerminalMode, WriteLogger,
};
use time::{macros::format_description, Date, Month, PrimitiveDateTime, Time};

const VEHICLE_COUNT_HEADER: &str = "Veh. No.,Date,Time,Channel,Class,Speed";

const LOG: &str = "log.txt";

/* Not sure if this will be needed, but these are the names of the 15 classifications from the FWA.
   See:
    * <https://www.fhwa.dot.gov/policyinformation/vehclass.cfm>
    * <https://www.fhwa.dot.gov/policyinformation/tmguide/tmg_2013/vehicle-types.cfm>
    * <https://www.fhwa.dot.gov/publications/research/infrastructure/pavements/ltpp/13091/002.cfm>
*/
#[derive(Debug, Clone)]
enum VehicleClass {
    Motorcycles,                        // 1
    PassengerCars,                      // 2
    OtherFourTireSingleUnitVehicles,    // 3
    Buses,                              // 4
    TwoAxleSixTireSingleUnitTrucks,     // 5
    ThreeAxleSingleUnitTrucks,          // 6
    FourOrMoreAxleSingleUnitTrucks,     // 7
    FourOrFewerAxleSingleTrailerTrucks, // 8
    FiveAxleSingleTrailerTrucks,        // 9
    SixOrMoreAxleSingleTrailerTrucks,   // 10
    FiveOrFewerAxleMultiTrailerTrucks,  // 11
    SixAxleMultiTrailerTrucks,          // 12
    SevenOrMoreAxleMultiTrailerTrucks,  // 13
    UnclassifiedVehicle,                // 15 (there is an "Unused" class group at 14)
}

impl VehicleClass {
    fn from_num(num: u8) -> Result<Self, String> {
        match num {
            1 => Ok(VehicleClass::Motorcycles),
            2 => Ok(VehicleClass::PassengerCars),
            3 => Ok(VehicleClass::OtherFourTireSingleUnitVehicles),
            4 => Ok(VehicleClass::Buses),
            5 => Ok(VehicleClass::TwoAxleSixTireSingleUnitTrucks),
            6 => Ok(VehicleClass::ThreeAxleSingleUnitTrucks),
            7 => Ok(VehicleClass::FourOrMoreAxleSingleUnitTrucks),
            8 => Ok(VehicleClass::FourOrFewerAxleSingleTrailerTrucks),
            9 => Ok(VehicleClass::FiveAxleSingleTrailerTrucks),
            10 => Ok(VehicleClass::SixOrMoreAxleSingleTrailerTrucks),
            11 => Ok(VehicleClass::FiveOrFewerAxleMultiTrailerTrucks),
            12 => Ok(VehicleClass::SixAxleMultiTrailerTrucks),
            13 => Ok(VehicleClass::SevenOrMoreAxleMultiTrailerTrucks),
            0 | 14 => Ok(VehicleClass::UnclassifiedVehicle), // TODO: verify this
            other => Err(format!("no such vehicle class '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CountType {
    FifteenMinuteBicycle,    // Eco-Counter
    FifteenMinutePedestrian, // Eco-Counter
    Vehicle, // this is the raw data that all of the other StarNext types get built from
}

impl CountType {
    fn from_location(path: &Path) -> Result<CountType, String> {
        // Get the directory immediately above the file.
        let parent = path
            .parent()
            .unwrap()
            .components()
            .last()
            .unwrap()
            .as_os_str()
            .to_str()
            .unwrap();

        match parent.to_lowercase().as_str() {
            "15minutebicycle" => Ok(CountType::FifteenMinuteBicycle),
            "15minutepedestrian" => Ok(CountType::FifteenMinutePedestrian),
            "vehicles" => Ok(CountType::Vehicle),
            _ => Err(format!("No matching count type for directory {path:?}")),
        }
    }

    fn from_header(path: &Path, location_count_type: CountType) -> Result<CountType, String> {
        // `location_count_type` is what we expect this file to be, based off its location. We use
        // this because different types of counts can have a variable number of metadata rows.

        let file = File::open(path).unwrap();
        let mut rdr = create_reader(&file);
        let header = rdr
            .records()
            .skip(num_metadata_rows_to_skip(location_count_type))
            .take(1)
            .last()
            .unwrap()
            .unwrap()
            .iter()
            .map(|x| x.trim().to_string())
            .collect::<Vec<String>>()
            .join(",");

        match header.as_str() {
            VEHICLE_COUNT_HEADER => Ok(CountType::Vehicle),
            _ => Err(format!("No matching count type for header in {path:?}.")),
        }
    }
}

// Vehicle Counts - the raw, unbinned data
#[derive(Debug, Clone)]
struct VehicleCount {
    date: Date,
    time: Time,
    channel: u8,
    class: VehicleClass,
    speed: f32,
}

impl VehicleCount {
    fn new(date: Date, time: Time, channel: u8, class: u8, speed: f32) -> Self {
        let class = VehicleClass::from_num(class).unwrap();
        VehicleCount {
            date,
            time,
            channel,
            class,
            speed,
        }
    }
}

// The first few lines of CSVs contain metadata, then header, then data rows.
#[derive(Debug, Clone)]
struct CountMetadata {
    start_datetime: PrimitiveDateTime,
    site_code: usize,
    station_id: Option<usize>,
}

// 14 speed ranges: 0-15, 5-mph increments to 75, more than 75
// In the TC_SPECOUNT table, the fieldnames for this are S1 to S14.
#[derive(Debug)]
enum SpeedBins {
    ZeroToFourteen,
    FifteenToNineteen,
    TwentyToTwentyFour,
    TwentyFiveToTwentyNine,
    ThirtyToThirtyFour,
    ThirtyFiveToThirtyNine,
    FourtyToFourtyFour,
    FourtyFiveToFourtyNine,
    FiftyToFiftyFour,
    FiftyFiveToFiftyNine,
    SixtyToSixtyFour,
    SixtyFiveToSixtyNine,
    SeventyToSeventyFour,
    SeventyFivePlus,
}

impl SpeedBins {
    fn bin(speed: f32) -> Result<Self, String> {
        if speed.is_sign_negative() {
            return Err(format!("invalid speed '{speed}'"));
        }

        // TODO: This just rounds down to integer. May have to do proper rounding to nearest int.
        // If so, will need to adjust tests.
        let speed = speed as i32;
        match speed {
            0..=14 => Ok(SpeedBins::ZeroToFourteen),
            15..=19 => Ok(SpeedBins::FifteenToNineteen),
            20..=24 => Ok(SpeedBins::TwentyToTwentyFour),
            25..=29 => Ok(SpeedBins::TwentyFiveToTwentyNine),
            30..=34 => Ok(SpeedBins::ThirtyToThirtyFour),
            35..=39 => Ok(SpeedBins::ThirtyFiveToThirtyNine),
            40..=44 => Ok(SpeedBins::FourtyToFourtyFour),
            45..=49 => Ok(SpeedBins::FourtyFiveToFourtyNine),
            50..=54 => Ok(SpeedBins::FiftyToFiftyFour),
            55..=59 => Ok(SpeedBins::FiftyFiveToFiftyNine),
            60..=64 => Ok(SpeedBins::SixtyToSixtyFour),
            65..=69 => Ok(SpeedBins::SixtyFiveToSixtyNine),
            70..=74 => Ok(SpeedBins::SeventyToSeventyFour),
            75.. => Ok(SpeedBins::SeventyFivePlus),
            other => Err(format!("invalid speed '{other}'")),
        }
    }
}

fn main() {
    // Load file containing environment variables, panic if it doesn't exist.
    dotenvy::dotenv().expect("Unable to load .env file.");

    // Get env var for path where CSVs will be, panic if it doesn't exist.
    let data_dir = env::var("DATA_DIR").expect("Unable to data directory path from .env file.");

    // Set up logging, panic if it fails.
    let config = ConfigBuilder::new().set_time_format_rfc3339().build();
    CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Debug,
            config.clone(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        WriteLogger::new(
            LevelFilter::Info,
            config,
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(format!("{data_dir}/{LOG}"))
                .expect("Could not open log file."),
        ),
    ])
    .expect("Could not configure logging.");

    // Oracle env vars
    let username = match env::var("USERNAME") {
        Ok(v) => v,
        Err(e) => {
            error!("Unable to load username from .env file: {e}.");
            return;
        }
    };
    let password = match env::var("PASSWORD") {
        Ok(v) => v,
        Err(e) => {
            error!("Unable to load password from .env file: {e}.");
            return;
        }
    };

    let paths = collect_paths(&data_dir.into(), vec![]);

    // TODO: this will probably need to be wrapped in other type in order to accept both StarNext
    // and EcoCounter counts
    let mut counts = vec![];

    for path in paths {
        let data_file = match File::open(&path) {
            Ok(v) => v,
            Err(_) => {
                error!("Unable to open {path:?}. File not processed.");
                continue;
            }
        };

        let count_type_from_location = CountType::from_location(&path).unwrap();
        let count_type_from_header =
            CountType::from_header(&path, count_type_from_location).unwrap();

        if count_type_from_location != count_type_from_header {
            error!("{path:?}: Mismatch in count types between the file location and the header in that file. File not processed.");
        } else {
            let file_counts = extract_counts(data_file, &path, count_type_from_location);
            counts.extend(file_counts);
        }
    }

    // mostly just debugging
    for count in counts {
        dbg!(&count);
    }
}

/// Collect all the file paths to extract data from.
fn collect_paths(dir: &PathBuf, mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();

        // Ignore the log file.
        if path.file_name().unwrap().to_str() == Some(LOG) {
            continue;
        }

        if path.is_dir() {
            paths = collect_paths(&path, paths);
        } else {
            paths.push(path);
        }
    }
    paths
}

/// Extract counts from a file.
fn extract_counts(data_file: File, path: &Path, count_type: CountType) -> Vec<VehicleCount> {
    // Create CSV reader over file
    let mut rdr = create_reader(&data_file);

    info!("Extracting data from {path:?}, a {count_type:?} count.");

    // Iterate through data rows (skipping metadata rows + 1 for header).
    let mut counts = vec![];
    for row in rdr
        .records()
        .skip(num_metadata_rows_to_skip(count_type) + 1)
    {
        // Parse date.
        let date_format = format_description!("[month padding:none]/[day padding:none]/[year]");
        let date_col = &row.as_ref().unwrap()[1];
        let count_date = Date::parse(date_col, &date_format).unwrap();

        // Parse time.
        let time_format =
            format_description!("[hour padding:none repr:12]:[minute]:[second] [period]");
        let time_col = &row.as_ref().unwrap()[2];
        let count_time = Time::parse(time_col, &time_format).unwrap();

        if count_type == CountType::Vehicle {
            let count = VehicleCount::new(
                count_date,
                count_time,
                row.as_ref().unwrap()[3].parse().unwrap(),
                row.as_ref().unwrap()[4].parse().unwrap(),
                row.as_ref().unwrap()[5].parse().unwrap(),
            );
            counts.push(count);
        }
    }
    counts
}

/// Put time into four bins per hour.
fn time_bin(time: Time) -> Result<Time, String> {
    let time = time.replace_second(0).unwrap();
    match time.minute() {
        0..=14 => Ok(time.replace_minute(0).unwrap()),
        15..=29 => Ok(time.replace_minute(15).unwrap()),
        30..=44 => Ok(time.replace_minute(30).unwrap()),
        45..=59 => Ok(time.replace_minute(45).unwrap()),
        _ => Err("Invalid minute".to_string()),
    }
}

/// Create CSV reader from file.
fn create_reader(file: &File) -> Reader<&File> {
    ReaderBuilder::new()
        .has_headers(false)
        .trim(csv::Trim::All)
        .flexible(true)
        .from_reader(file)
}

fn num_metadata_rows_to_skip(count_type: CountType) -> usize {
    match count_type {
        CountType::Vehicle => 3,
        _ => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_type_vehicle_ok() {
        let path = Path::new("test_files/vehicles/rc-166905-ew-40972-35.txt");
        let ct_from_location = CountType::from_location(path).unwrap();
        let ct_from_header = CountType::from_header(path, ct_from_location).unwrap();
        assert_eq!(&ct_from_location, &ct_from_header);
        assert_eq!(ct_from_location, CountType::Vehicle);
    }

    #[test]
    fn extract_counts_gets_correct_number_of_counts() {
        let path = Path::new("test_files/vehicles/rc-166905-ew-40972-35.txt");
        let ct_from_location = CountType::from_location(path).unwrap();
        let data_file = File::open(path).unwrap();
        let counts = extract_counts(data_file, path, ct_from_location);
        assert_eq!(counts.len(), 8706);
    }

    #[test]
    fn time_binning_is_correct() {
        // 1st 15-minute bin
        let time = Time::from_hms(10, 0, 0).unwrap();

        let binned = time_bin(time).unwrap();
        assert_eq!(binned, Time::from_hms(10, 0, 0).unwrap());
        assert_ne!(binned, Time::from_hms(10, 10, 0).unwrap());

        let time = Time::from_hms(10, 14, 00).unwrap();
        let binned = time_bin(time).unwrap();
        assert_eq!(binned, Time::from_hms(10, 0, 0).unwrap());

        // 2nd 15-minute bin
        let time = Time::from_hms(10, 25, 00).unwrap();

        let binned = time_bin(time).unwrap();
        assert_eq!(binned, Time::from_hms(10, 15, 0).unwrap());

        let time = Time::from_hms(10, 29, 00).unwrap();

        let binned = time_bin(time).unwrap();
        assert_eq!(binned, Time::from_hms(10, 15, 0).unwrap());

        // 3rd 15-minute bin
        let time = Time::from_hms(10, 31, 00).unwrap();

        let binned = time_bin(time).unwrap();
        assert_eq!(binned, Time::from_hms(10, 30, 0).unwrap());

        let time = Time::from_hms(10, 44, 00).unwrap();

        let binned = time_bin(time).unwrap();
        assert_eq!(binned, Time::from_hms(10, 30, 0).unwrap());

        // 4th 15-minute bin
        let time = Time::from_hms(10, 45, 00).unwrap();

        let binned = time_bin(time).unwrap();
        assert_eq!(binned, Time::from_hms(10, 45, 0).unwrap());

        let time = Time::from_hms(10, 59, 00).unwrap();

        let binned = time_bin(time).unwrap();
        assert_eq!(binned, Time::from_hms(10, 45, 0).unwrap());
    }

    #[test]
    fn speed_binning_is_correct() {
        assert!(SpeedBins::bin(-0.1).is_err());
        assert!(SpeedBins::bin(-0.0).is_err());
        assert!(matches!(SpeedBins::bin(0.0), Ok(SpeedBins::ZeroToFourteen)));
        assert!(matches!(SpeedBins::bin(0.1), Ok(SpeedBins::ZeroToFourteen)));
        assert!(matches!(
            SpeedBins::bin(14.9),
            Ok(SpeedBins::ZeroToFourteen)
        ));
        assert!(matches!(
            SpeedBins::bin(15.0),
            Ok(SpeedBins::FifteenToNineteen)
        ));
        assert!(matches!(
            SpeedBins::bin(19.9999),
            Ok(SpeedBins::FifteenToNineteen)
        ));
        assert!(matches!(
            SpeedBins::bin(20.01),
            Ok(SpeedBins::TwentyToTwentyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(24.9),
            Ok(SpeedBins::TwentyToTwentyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(25.0),
            Ok(SpeedBins::TwentyFiveToTwentyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(29.9),
            Ok(SpeedBins::TwentyFiveToTwentyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(30.0),
            Ok(SpeedBins::ThirtyToThirtyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(34.9),
            Ok(SpeedBins::ThirtyToThirtyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(35.0),
            Ok(SpeedBins::ThirtyFiveToThirtyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(39.9),
            Ok(SpeedBins::ThirtyFiveToThirtyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(40.0),
            Ok(SpeedBins::FourtyToFourtyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(44.9),
            Ok(SpeedBins::FourtyToFourtyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(45.0),
            Ok(SpeedBins::FourtyFiveToFourtyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(49.9),
            Ok(SpeedBins::FourtyFiveToFourtyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(50.0),
            Ok(SpeedBins::FiftyToFiftyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(54.9),
            Ok(SpeedBins::FiftyToFiftyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(55.0),
            Ok(SpeedBins::FiftyFiveToFiftyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(59.9),
            Ok(SpeedBins::FiftyFiveToFiftyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(60.0),
            Ok(SpeedBins::SixtyToSixtyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(64.9),
            Ok(SpeedBins::SixtyToSixtyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(65.0),
            Ok(SpeedBins::SixtyFiveToSixtyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(69.9),
            Ok(SpeedBins::SixtyFiveToSixtyNine)
        ));
        assert!(matches!(
            SpeedBins::bin(70.0),
            Ok(SpeedBins::SeventyToSeventyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(74.9),
            Ok(SpeedBins::SeventyToSeventyFour)
        ));
        assert!(matches!(
            SpeedBins::bin(75.0),
            Ok(SpeedBins::SeventyFivePlus)
        ));
        assert!(matches!(
            SpeedBins::bin(75.9),
            Ok(SpeedBins::SeventyFivePlus)
        ));
        assert!(matches!(
            SpeedBins::bin(100.0),
            Ok(SpeedBins::SeventyFivePlus)
        ));
        assert!(matches!(
            SpeedBins::bin(120.0),
            Ok(SpeedBins::SeventyFivePlus)
        ));
    }
}
