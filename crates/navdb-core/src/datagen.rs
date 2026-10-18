//! Data Generation tool: per-column random value generators.

use rand::seq::SliceRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::model::ColumnDef;
use crate::value::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Generator {
    /// Skip column (let DB fill default / auto increment).
    Skip,
    Null,
    Sequence { start: i64, step: i64 },
    IntRange { min: i64, max: i64 },
    FloatRange { min: f64, max: f64, decimals: u32 },
    Bool,
    FirstName,
    LastName,
    FullName,
    Email,
    Phone,
    City,
    Country,
    Address,
    Company,
    Uuid,
    Date { from_year: i32, to_year: i32 },
    DateTime { from_year: i32, to_year: i32 },
    Lorem { words: usize },
    RandomString { len: usize },
    Fixed(String),
    List(Vec<String>),
}

impl Generator {
    pub const NAMES: [&'static str; 22] = [
        "Skip", "Null", "Sequence", "Integer range", "Decimal range", "Boolean", "First name", "Last name", "Full name",
        "Email", "Phone", "City", "Country", "Address", "Company", "UUID", "Date", "Date/Time", "Lorem ipsum",
        "Random string", "Fixed value", "Value list",
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Generator::Skip => "Skip",
            Generator::Null => "Null",
            Generator::Sequence { .. } => "Sequence",
            Generator::IntRange { .. } => "Integer range",
            Generator::FloatRange { .. } => "Decimal range",
            Generator::Bool => "Boolean",
            Generator::FirstName => "First name",
            Generator::LastName => "Last name",
            Generator::FullName => "Full name",
            Generator::Email => "Email",
            Generator::Phone => "Phone",
            Generator::City => "City",
            Generator::Country => "Country",
            Generator::Address => "Address",
            Generator::Company => "Company",
            Generator::Uuid => "UUID",
            Generator::Date { .. } => "Date",
            Generator::DateTime { .. } => "Date/Time",
            Generator::Lorem { .. } => "Lorem ipsum",
            Generator::RandomString { .. } => "Random string",
            Generator::Fixed(_) => "Fixed value",
            Generator::List(_) => "Value list",
        }
    }

    pub fn from_name(n: &str) -> Generator {
        match n {
            "Skip" => Generator::Skip,
            "Null" => Generator::Null,
            "Sequence" => Generator::Sequence { start: 1, step: 1 },
            "Integer range" => Generator::IntRange { min: 0, max: 1000 },
            "Decimal range" => Generator::FloatRange { min: 0.0, max: 1000.0, decimals: 2 },
            "Boolean" => Generator::Bool,
            "First name" => Generator::FirstName,
            "Last name" => Generator::LastName,
            "Full name" => Generator::FullName,
            "Email" => Generator::Email,
            "Phone" => Generator::Phone,
            "City" => Generator::City,
            "Country" => Generator::Country,
            "Address" => Generator::Address,
            "Company" => Generator::Company,
            "UUID" => Generator::Uuid,
            "Date" => Generator::Date { from_year: 2015, to_year: 2026 },
            "Date/Time" => Generator::DateTime { from_year: 2015, to_year: 2026 },
            "Lorem ipsum" => Generator::Lorem { words: 8 },
            "Random string" => Generator::RandomString { len: 12 },
            "Fixed value" => Generator::Fixed(String::new()),
            _ => Generator::List(vec!["A".into(), "B".into(), "C".into()]),
        }
    }

    /// Pick a sensible generator from column name and type.
    pub fn guess(c: &ColumnDef) -> Generator {
        let n = c.name.to_ascii_lowercase();
        let t = c.data_type.to_ascii_lowercase();
        if c.auto_increment {
            return Generator::Skip;
        }
        if n.contains("email") || n.contains("mail") {
            return Generator::Email;
        }
        if n.contains("first") && n.contains("name") {
            return Generator::FirstName;
        }
        if (n.contains("last") || n.contains("sur")) && n.contains("name") {
            return Generator::LastName;
        }
        if n == "name" || n.contains("full_name") || n.contains("fullname") || n == "username" {
            return Generator::FullName;
        }
        if n.contains("phone") || n.contains("tel") || n.contains("mobile") {
            return Generator::Phone;
        }
        if n.contains("city") {
            return Generator::City;
        }
        if n.contains("country") {
            return Generator::Country;
        }
        if n.contains("address") || n.contains("street") {
            return Generator::Address;
        }
        if n.contains("company") || n.contains("org") {
            return Generator::Company;
        }
        if n.contains("uuid") || n == "guid" || t.contains("uuid") {
            return Generator::Uuid;
        }
        if t.contains("bool") || t == "tinyint(1)" || t == "bit(1)" {
            return Generator::Bool;
        }
        if t.contains("timestamp") || t.contains("datetime") {
            return Generator::DateTime { from_year: 2015, to_year: 2026 };
        }
        if t.starts_with("date") {
            return Generator::Date { from_year: 2015, to_year: 2026 };
        }
        if t.contains("int") || t.contains("serial") {
            return Generator::IntRange { min: 1, max: 10_000 };
        }
        if t.contains("dec") || t.contains("num") || t.contains("float") || t.contains("double") || t.contains("real") || t.contains("money") {
            return Generator::FloatRange { min: 0.0, max: 10_000.0, decimals: 2 };
        }
        if t.contains("json") {
            return Generator::Fixed("{}".into());
        }
        if t.contains("text") {
            return Generator::Lorem { words: 12 };
        }
        let len = t
            .split(|c| c == '(' || c == ')')
            .nth(1)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(16)
            .min(24);
        Generator::RandomString { len: len.max(1) }
    }
}

const FIRST: &[&str] = &["Ali", "Ayşe", "Mehmet", "Zeynep", "John", "Emma", "Liam", "Olivia", "Noah", "Mia", "Lucas", "Elif", "Can", "Deniz", "Sofia", "Hiro"];
const LAST: &[&str] = &["Yılmaz", "Kaya", "Demir", "Smith", "Johnson", "Brown", "Garcia", "Müller", "Rossi", "Tanaka", "Öztürk", "Wilson", "Martin", "Lee"];
const CITY: &[&str] = &["İstanbul", "Ankara", "İzmir", "London", "Berlin", "Paris", "Tokyo", "New York", "Madrid", "Rome", "Amsterdam", "Toronto"];
const COUNTRY: &[&str] = &["Türkiye", "United Kingdom", "Germany", "France", "Japan", "United States", "Spain", "Italy", "Netherlands", "Canada"];
const STREET: &[&str] = &["Atatürk Cd.", "Main St.", "High St.", "Park Ave.", "Bağdat Cd.", "Oak Rd.", "Elm St.", "Station Rd."];
const COMPANY: &[&str] = &["Acme", "Globex", "Initech", "Umbrella", "Hooli", "Stark", "Wayne", "Wonka", "Soylent", "Cyberdyne"];
const LOREM: &[&str] = &[
    "lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "sed", "do", "eiusmod", "tempor",
    "incididunt", "ut", "labore", "et", "dolore", "magna", "aliqua", "enim", "minim", "veniam", "quis", "nostrud",
];

fn pick<R: Rng>(rng: &mut R, list: &[&str]) -> String {
    list.choose(rng).copied().unwrap_or("").to_string()
}

fn ascii(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'ı' => 'i',
            'İ' => 'I',
            'ş' | 'Ş' => 's',
            'ç' | 'Ç' => 'c',
            'ğ' | 'Ğ' => 'g',
            'ü' | 'Ü' => 'u',
            'ö' | 'Ö' => 'o',
            c => c,
        })
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

pub fn generate<R: Rng>(g: &Generator, row: usize, rng: &mut R) -> Option<Value> {
    Some(match g {
        Generator::Skip => return None,
        Generator::Null => Value::Null,
        Generator::Sequence { start, step } => Value::Int(start + step * row as i64),
        Generator::IntRange { min, max } => Value::Int(if max > min { rng.gen_range(*min..=*max) } else { *min }),
        Generator::FloatRange { min, max, decimals } => {
            let v = if max > min { rng.gen_range(*min..*max) } else { *min };
            let f = 10f64.powi(*decimals as i32);
            Value::Float((v * f).round() / f)
        }
        Generator::Bool => Value::Int(rng.gen_range(0..=1)),
        Generator::FirstName => Value::Text(pick(rng, FIRST)),
        Generator::LastName => Value::Text(pick(rng, LAST)),
        Generator::FullName => Value::Text(format!("{} {}", pick(rng, FIRST), pick(rng, LAST))),
        Generator::Email => Value::Text(format!(
            "{}.{}{}@example.{}",
            ascii(&pick(rng, FIRST)),
            ascii(&pick(rng, LAST)),
            rng.gen_range(1..999),
            ["com", "org", "net", "dev"].choose(rng).unwrap()
        )),
        Generator::Phone => Value::Text(format!(
            "+{} {:03} {:03} {:04}",
            rng.gen_range(1..99),
            rng.gen_range(100..999),
            rng.gen_range(100..999),
            rng.gen_range(0..9999)
        )),
        Generator::City => Value::Text(pick(rng, CITY)),
        Generator::Country => Value::Text(pick(rng, COUNTRY)),
        Generator::Address => Value::Text(format!("{} {} No:{}", pick(rng, STREET), pick(rng, CITY), rng.gen_range(1..300))),
        Generator::Company => Value::Text(format!("{} {}", pick(rng, COMPANY), ["Inc.", "Ltd.", "A.Ş.", "GmbH", "LLC"].choose(rng).unwrap())),
        Generator::Uuid => Value::Text(uuid::Uuid::new_v4().to_string()),
        Generator::Date { from_year, to_year } => {
            let y = rng.gen_range(*from_year..=(*to_year).max(*from_year));
            Value::Text(format!("{:04}-{:02}-{:02}", y, rng.gen_range(1..=12), rng.gen_range(1..=28)))
        }
        Generator::DateTime { from_year, to_year } => {
            let y = rng.gen_range(*from_year..=(*to_year).max(*from_year));
            Value::Text(format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                y,
                rng.gen_range(1..=12),
                rng.gen_range(1..=28),
                rng.gen_range(0..24),
                rng.gen_range(0..60),
                rng.gen_range(0..60)
            ))
        }
        Generator::Lorem { words } => {
            let w: Vec<String> = (0..(*words).max(1)).map(|_| pick(rng, LOREM)).collect();
            let mut s = w.join(" ");
            if let Some(f) = s.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            Value::Text(s + ".")
        }
        Generator::RandomString { len } => {
            const CH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
            Value::Text((0..*len).map(|_| CH[rng.gen_range(0..CH.len())] as char).collect())
        }
        Generator::Fixed(s) => Value::Text(s.clone()),
        Generator::List(items) => {
            if items.is_empty() {
                Value::Null
            } else {
                Value::Text(items[rng.gen_range(0..items.len())].clone())
            }
        }
    })
}

/// Generate `count` rows. Returns (column names, rows) for non-skipped columns.
pub fn generate_rows(cols: &[(String, Generator)], count: usize, start_row: usize) -> (Vec<String>, Vec<Vec<Value>>) {
    let mut rng = rand::thread_rng();
    let names: Vec<String> = cols.iter().filter(|(_, g)| *g != Generator::Skip).map(|(n, _)| n.clone()).collect();
    let rows = (0..count)
        .map(|i| cols.iter().filter_map(|(_, g)| generate(g, start_row + i, &mut rng)).collect())
        .collect();
    (names, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses() {
        assert_eq!(Generator::guess(&ColumnDef::new("email", "varchar(100)")), Generator::Email);
        let mut id = ColumnDef::new("id", "int");
        id.auto_increment = true;
        assert_eq!(Generator::guess(&id), Generator::Skip);
        assert!(matches!(Generator::guess(&ColumnDef::new("price", "decimal(10,2)")), Generator::FloatRange { .. }));
    }

    #[test]
    fn rows() {
        let (n, r) = generate_rows(
            &[("id".into(), Generator::Skip), ("seq".into(), Generator::Sequence { start: 10, step: 2 }), ("e".into(), Generator::Email)],
            3,
            0,
        );
        assert_eq!(n, vec!["seq", "e"]);
        assert_eq!(r[2][0], Value::Int(14));
        assert!(r[0][1].to_string().contains('@'));
    }
}
