/// Ordered list of schema migrations. Never edit a migration that has shipped;
/// append a new one and bump [`SCHEMA_VERSION`].
pub(crate) const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../migrations/0001_init.sql")),
    (2, include_str!("../../migrations/0002_attachment_data.sql")),
];

pub const SCHEMA_VERSION: i64 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_ascending_and_unique() {
        let mut previous = 0;
        for (version, sql) in MIGRATIONS {
            assert!(*version > previous, "migration {version} is out of order");
            assert!(!sql.trim().is_empty(), "migration {version} is empty");
            previous = *version;
        }
        assert_eq!(previous, SCHEMA_VERSION);
    }
}
