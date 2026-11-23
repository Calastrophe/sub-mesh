//! One module per dashboard page, plus the page list.

mod daemons;
mod logs;
mod quota;
mod usage;
mod users;

pub use daemons::DaemonsPage;
pub use logs::LogsPage;
pub use quota::QuotaPage;
pub use usage::UsagePage;
pub use users::UsersPage;

/// A dashboard page, chosen by the URL fragment (`#quota`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Quota,
    Usage,
    Logs,
    Daemons,
    Users,
}

impl Page {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::Usage => "usage",
            Self::Logs => "logs",
            Self::Daemons => "daemons",
            Self::Users => "users",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Quota => "Quota",
            Self::Usage => "Usage",
            Self::Logs => "Logs",
            Self::Daemons => "Daemons",
            Self::Users => "Users",
        }
    }

    /// Read a URL fragment like `#quota` or `#/quota`. Anything unknown is
    /// the default page, Quota.
    pub fn from_hash(hash: &str) -> Self {
        match hash.trim_start_matches('#').trim_start_matches('/') {
            "usage" => Self::Usage,
            "logs" => Self::Logs,
            "daemons" => Self::Daemons,
            "users" => Self::Users,
            _ => Self::Quota,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_round_trips() {
        for page in [
            Page::Quota,
            Page::Usage,
            Page::Logs,
            Page::Daemons,
            Page::Users,
        ] {
            assert_eq!(Page::from_hash(&format!("#{}", page.slug())), page);
        }
        assert_eq!(Page::from_hash("#/users"), Page::Users);
        assert_eq!(Page::from_hash(""), Page::Quota);
        assert_eq!(Page::from_hash("#nope"), Page::Quota);
    }
}
