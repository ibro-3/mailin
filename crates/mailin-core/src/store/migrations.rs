/// Ordered list of schema migrations. Never edit a migration that has shipped;
/// append a new one and bump [`SCHEMA_VERSION`].
pub(crate) const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../migrations/0001_init.sql")),
    (2, include_str!("../../migrations/0002_attachment_data.sql")),
    (3, include_str!("../../migrations/0003_templates.sql")),
    (4, include_str!("../../migrations/0004_contacts.sql")),
    (
        5,
        include_str!("../../migrations/0005_scheduled_actions.sql"),
    ),
    (6, include_str!("../../migrations/0006_rules.sql")),
    (7, include_str!("../../migrations/0007_virtual_folders.sql")),
    (8, include_str!("../../migrations/0008_pgp_keys.sql")),
    (9, include_str!("../../migrations/0009_calendars.sql")),
    (10, include_str!("../../migrations/0010_plugins.sql")),
    (11, include_str!("../../migrations/0011_threading.sql")),
    (12, include_str!("../../migrations/0012_pgp_mime.sql")),
    (13, include_str!("../../migrations/0013_references.sql")),
    (
        14,
        include_str!("../../migrations/0014_folder_subscribed.sql"),
    ),
];

pub const SCHEMA_VERSION: i64 = 14;

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
