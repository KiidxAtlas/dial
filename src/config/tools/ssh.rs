//! `[ssh]` host key policy and connection timing.

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SshConfig {
    pub host_key_checking: HostKeyChecking,
    pub connect_timeout_secs: u64,
    pub control_persist_secs: u64,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host_key_checking: HostKeyChecking::AcceptNew,
            connect_timeout_secs: 15,
            control_persist_secs: 3600,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostKeyChecking {
    Yes,
    AcceptNew,
    No,
}

impl HostKeyChecking {
    pub(crate) fn as_option(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::AcceptNew => "accept-new",
            Self::No => "no",
        }
    }
}
