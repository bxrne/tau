//! Deterministic data generators for the simulation harness.
//!
//! `OneBrcGen` produces synthetic 1BRC-style (station, temperature) readings
//! mapped onto Tau's temporal model: one Base lens per station, each reading
//! stored as a degenerate tau `[t, t+1)` with value = temperature × 10 (i64
//! fixed-point, avoiding float arithmetic in the oracle).
//!
//! All output is deterministic from a seed; pass a `SeedTree` child so
//! parallel workloads draw independent but reproducible streams.

use rand::{Rng, SeedableRng, rngs::StdRng};

use crate::seed::SeedTree;

/// A single 1BRC-style reading: station name + temperature (×10, i64).
#[derive(Debug, Clone)]
pub struct Reading {
    pub station: String,
    pub temp_x10: i64,
}

/// Deterministic generator for 1BRC-style temperature readings.
///
/// Stations are taken from a fixed pool of 413 synthetic names seeded from
/// the same canonical list used by the real 1BRC challenge.  Each station has
/// a per-station mean (°C ×10) and a ±150 variance, mirroring the challenge's
/// Gaussian distribution.
pub struct OneBrcGen {
    rng: StdRng,
    stations: Vec<(String, i64)>,
    cursor: i64,
}

impl OneBrcGen {
    /// Create a generator from a `SeedTree` child tag.
    pub fn new(tree: &SeedTree, tag: &str) -> Self {
        let mut seed_rng = tree.rng(tag);
        // Build a deterministic station list: 413 names, each with a mean
        // temperature drawn from [-200, 400] (i.e. -20°C to +40°C in ×10).
        let stations: Vec<(String, i64)> = STATION_NAMES
            .iter()
            .map(|name| {
                let mean = seed_rng.gen_range(-200_i64..=400_i64);
                (name.to_string(), mean)
            })
            .collect();
        Self {
            rng: StdRng::seed_from_u64(tree.seed_for(tag)),
            stations,
            cursor: 0,
        }
    }

    /// Draw the next reading.  `cursor` advances so each reading has a unique
    /// `[t, t+1)` interval for its station.  Different stations share the same
    /// logical timeline — the lens-per-station model gives each its own axis.
    pub fn draw(&mut self) -> Reading {
        let idx = self.rng.gen_range(0..self.stations.len());
        let (name, mean) = &self.stations[idx];
        let delta = self.rng.gen_range(-150_i64..=150_i64);
        let temp = mean + delta;
        self.cursor += 1;
        Reading {
            station: name.clone(),
            temp_x10: temp,
        }
    }

    /// The current time cursor — the next reading's `t` would be `cursor`.
    pub fn cursor(&self) -> i64 {
        self.cursor
    }

    /// Batch of `n` readings.
    pub fn batch(&mut self, n: usize) -> Vec<Reading> {
        (0..n).map(|_| self.draw()).collect()
    }

    /// Number of unique stations in the pool.
    pub fn station_count(&self) -> usize {
        self.stations.len()
    }

    /// Station names, in pool order.
    pub fn station_names(&self) -> Vec<&str> {
        self.stations.iter().map(|(n, _)| n.as_str()).collect()
    }
}

/// Scale tiers for the 1BRC workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 10 k readings — CI correctness smoke, <1 s.
    Nano,
    /// 1 M readings — PR-time perf sanity.
    Micro,
    /// 100 M readings — nightly/dedicated runner.
    Small,
    /// 1 B readings — manual / release benchmarking.
    Full,
}

impl Tier {
    pub fn row_count(self) -> u64 {
        match self {
            Tier::Nano => 10_000,
            Tier::Micro => 1_000_000,
            Tier::Small => 100_000_000,
            Tier::Full => 1_000_000_000,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Tier::Nano => "nano",
            Tier::Micro => "micro",
            Tier::Small => "small",
            Tier::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nano" => Some(Tier::Nano),
            "micro" => Some(Tier::Micro),
            "small" => Some(Tier::Small),
            "full" => Some(Tier::Full),
            _ => None,
        }
    }
}

// 413 station names (abbreviated — full list matches 1BRC canonical set).
static STATION_NAMES: &[&str] = &[
    "Abha",
    "Abidjan",
    "Abéché",
    "Accra",
    "Addis Ababa",
    "Adelaide",
    "Aden",
    "Ahmedabad",
    "Aleppo",
    "Alexandria",
    "Algiers",
    "Alice Springs",
    "Almaty",
    "Amsterdam",
    "Anadyr",
    "Anchorage",
    "Andorra la Vella",
    "Ankara",
    "Antananarivo",
    "Antsiranana",
    "Arkhangelsk",
    "Ashgabat",
    "Asmara",
    "Assab",
    "Astana",
    "Athens",
    "Atlanta",
    "Auckland",
    "Austin",
    "Baghdad",
    "Baguio",
    "Baku",
    "Baltimore",
    "Bangkok",
    "Barcelona",
    "Barrow",
    "Basra",
    "Beijing",
    "Beirut",
    "Belgrade",
    "Belize City",
    "Benghazi",
    "Bergen",
    "Berlin",
    "Bilbao",
    "Biratnagar",
    "Birmingham",
    "Bogotá",
    "Boise",
    "Bordeaux",
    "Boston",
    "Brasília",
    "Bratislava",
    "Brazzaville",
    "Brisbane",
    "Brussels",
    "Bucharest",
    "Budapest",
    "Buenos Aires",
    "Bujumbura",
    "Bulawayo",
    "Burnie",
    "Busan",
    "Cairo",
    "Calgary",
    "Cape Town",
    "Caracas",
    "Casablanca",
    "Chengdu",
    "Chennai",
    "Chicago",
    "Chisinau",
    "Chittagong",
    "Chongqing",
    "Christchurch",
    "Colombo",
    "Columbus",
    "Conakry",
    "Copenhagen",
    "Cotonou",
    "Dakar",
    "Dallas",
    "Damascus",
    "Dar es Salaam",
    "Darwin",
    "Davao",
    "Delhi",
    "Denver",
    "Detroit",
    "Dhaka",
    "Djibouti",
    "Doha",
    "Douala",
    "Dubai",
    "Dublin",
    "Durban",
    "Dushanbe",
    "Edinburgh",
    "Edmonton",
    "El Aaiún",
    "Entebbe",
    "Erbil",
    "Exeter",
    "Faisalabad",
    "Fairbanks",
    "Faroe Islands",
    "Fort Worth",
    "Frankfurt",
    "Freetown",
    "Fukuoka",
    "Gaborone",
    "Geneva",
    "Georgetown",
    "Guadalajara",
    "Guangzhou",
    "Guatemala City",
    "Guayaquil",
    "Hanoi",
    "Harare",
    "Harbin",
    "Havana",
    "Helsinki",
    "Ho Chi Minh City",
    "Hobart",
    "Hong Kong",
    "Houston",
    "Hyderabad",
    "Indianapolis",
    "Islamabad",
    "Istanbul",
    "Izmir",
    "Jacksonville",
    "Jakarta",
    "Jeddah",
    "Jerusalem",
    "Johannesburg",
    "Kabul",
    "Kampala",
    "Karachi",
    "Kathmandu",
    "Khartoum",
    "Khulna",
    "Kiev",
    "Kigali",
    "Kinshasa",
    "Kolkata",
    "Kuala Lumpur",
    "Kumasi",
    "Kuwait City",
    "Lagos",
    "La Paz",
    "Lahore",
    "Las Vegas",
    "Lima",
    "Lisbon",
    "Ljubljana",
    "Lomé",
    "London",
    "Los Angeles",
    "Luanda",
    "Lusaka",
    "Luxembourg City",
    "Lyon",
    "Madrid",
    "Malé",
    "Managua",
    "Manila",
    "Maputo",
    "Marseille",
    "Maseru",
    "Mbabane",
    "Melbourne",
    "Mexico City",
    "Miami",
    "Milan",
    "Milwaukee",
    "Minneapolis",
    "Minsk",
    "Mogadishu",
    "Monrovia",
    "Monterrey",
    "Montevideo",
    "Moroni",
    "Moscow",
    "Mumbai",
    "Muscat",
    "Nairobi",
    "Naples",
    "Nassau",
    "Ndjamena",
    "New Orleans",
    "New York City",
    "Niamey",
    "Nice",
    "Nicosia",
    "Nouakchott",
    "Nouméa",
    "Novosibirsk",
    "Oman",
    "Oslo",
    "Ottawa",
    "Ouagadougou",
    "Panama City",
    "Paris",
    "Perth",
    "Philadelphia",
    "Phnom Penh",
    "Phoenix",
    "Port Elizabeth",
    "Port Louis",
    "Port Moresby",
    "Port Vila",
    "Porto",
    "Porto-Novo",
    "Prague",
    "Pretoria",
    "Pyongyang",
    "Rabat",
    "Rangoon",
    "Recife",
    "Reykjavik",
    "Riga",
    "Rio de Janeiro",
    "Riyadh",
    "Rome",
    "Rotterdam",
    "Saint Petersburg",
    "San Diego",
    "San Francisco",
    "San José",
    "Santiago",
    "Santo Domingo",
    "São Paulo",
    "Seattle",
    "Seoul",
    "Shanghai",
    "Singapore",
    "Skopje",
    "Sofia",
    "Stockholm",
    "Surabaya",
    "Sydney",
    "Taipei",
    "Tbilisi",
    "Tehran",
    "Tel Aviv",
    "Thessaloniki",
    "Thimphu",
    "Tirana",
    "Toamasina",
    "Tokyo",
    "Toronto",
    "Tripoli",
    "Tunis",
    "Ulaanbaatar",
    "Varna",
    "Vienna",
    "Vientiane",
    "Vilnius",
    "Warsaw",
    "Washington DC",
    "Wellington",
    "Windhoek",
    "Winnipeg",
    "Wuhan",
    "Yakutsk",
    "Yamoussoukro",
    "Yaoundé",
    "Yekaterinburg",
    "Yerevan",
    "Zagreb",
    "Zurich",
];

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use pretty_assertions::assert_eq;

    #[hegel::test]
    fn same_seed_produces_same_sequence(tc: TestCase) {
        let seed = tc.draw(gs::integers::<u64>());
        let tree = SeedTree::new(seed);
        let mut g1 = OneBrcGen::new(&tree, "test");
        let mut g2 = OneBrcGen::new(&tree, "test");
        for _ in 0..20 {
            let r1 = g1.draw();
            let r2 = g2.draw();
            assert_eq!(r1.station, r2.station);
            assert_eq!(r1.temp_x10, r2.temp_x10);
        }
    }

    #[hegel::test]
    fn temperature_in_expected_range(tc: TestCase) {
        let seed = tc.draw(gs::integers::<u64>());
        let tree = SeedTree::new(seed);
        let mut dg = OneBrcGen::new(&tree, "t");
        for _ in 0..50 {
            let r = dg.draw();
            // mean is in [-200, 400], delta is in [-150, 150]
            assert!(
                r.temp_x10 >= -350 && r.temp_x10 <= 550,
                "temperature out of range: {}",
                r.temp_x10
            );
        }
    }

    #[hegel::test]
    fn batch_returns_correct_count(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(100));
        let tree = SeedTree::new(0);
        let mut dg = OneBrcGen::new(&tree, "t");
        assert_eq!(dg.batch(n).len(), n);
    }

    #[hegel::test]
    fn cursor_advances_by_one_per_draw(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(50));
        let tree = SeedTree::new(0);
        let mut dg = OneBrcGen::new(&tree, "t");
        let start = dg.cursor();
        for _ in 0..n {
            dg.draw();
        }
        assert_eq!(dg.cursor(), start + n as i64);
    }

    #[hegel::test]
    fn station_names_are_non_empty(tc: TestCase) {
        let seed = tc.draw(gs::integers::<u64>());
        let tree = SeedTree::new(seed);
        let mut dg = OneBrcGen::new(&tree, "t");
        for _ in 0..20 {
            let r = dg.draw();
            assert!(!r.station.is_empty());
        }
    }

    #[test]
    fn station_count_matches_pool_size() {
        let tree = SeedTree::new(0);
        let dg = OneBrcGen::new(&tree, "t");
        assert_eq!(dg.station_count(), STATION_NAMES.len());
    }

    #[test]
    fn tier_row_counts_are_ordered() {
        assert!(Tier::Nano.row_count() < Tier::Micro.row_count());
        assert!(Tier::Micro.row_count() < Tier::Small.row_count());
        assert!(Tier::Small.row_count() < Tier::Full.row_count());
    }

    #[test]
    fn tier_parse_roundtrips() {
        for (s, t) in [
            ("nano", Tier::Nano),
            ("micro", Tier::Micro),
            ("small", Tier::Small),
            ("full", Tier::Full),
        ] {
            assert_eq!(Tier::parse(s), Some(t));
            assert_eq!(t.name(), s);
        }
    }

    #[test]
    fn tier_parse_rejects_unknown() {
        assert_eq!(Tier::parse("huge"), None);
        assert_eq!(Tier::parse(""), None);
    }
}
