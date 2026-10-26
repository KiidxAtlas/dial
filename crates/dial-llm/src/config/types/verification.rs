//! `[verification]`: configured checks, requirement contracts, runners and independent review.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationConfig {
    /// Record check results the model runs and gate `finish` on failed checks that were never repaired.
    pub enabled: bool,
    /// Also run focused checks on the harness's own initiative: a behavioral baseline before edits,
    /// checks at `finish` and after the turn, and up to `repair_attempts` automatic repair rounds.
    /// Off leaves verification to the model, which the system prompt requires before yielding.
    pub automatic_checks: bool,
    pub repair_attempts: u8,
    pub commands: Vec<VerificationCommand>,
    /// Names of configured commands that check behavioral correctness, not only syntax or buildability.
    /// Explicit operator declaration; only failing-before/passing-after runs qualify as repair evidence.
    pub behavioral_checks: Vec<String>,
    /// Operator-owned Python argv override; empty selects the owning project's runner.
    pub python_command: Vec<String>,
    /// Optional operator-declared requirement -> evidence contract. Check names refer to commands.
    pub requirements: BTreeMap<String, VerificationRequirement>,
    /// Refuse completion while operator-declared requirement evidence is missing or stale.
    pub strict_requirements: bool,
    /// Optional runner identity for configured checks; never an installation or execution grant.
    pub runners: BTreeMap<String, VerificationRunner>,
    /// Operator assertion, keyed by configured check name, that a deterministic, side-effect-free
    /// check's entire workspace inputs are declared; enables dependency-aware reuse of its result.
    pub inputs: BTreeMap<String, VerificationInputs>,
    /// Opt-in, bounded independent review of substantial or explicitly sensitive changes.
    pub review: IndependentReviewConfig,
}

impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            automatic_checks: false,
            repair_attempts: 0,
            commands: Vec::new(),
            behavioral_checks: Vec::new(),
            python_command: Vec::new(),
            requirements: BTreeMap::new(),
            strict_requirements: false,
            runners: BTreeMap::new(),
            inputs: BTreeMap::new(),
            review: IndependentReviewConfig::default(),
        }
    }
}

/// The selected executable/module, not arbitrary programs imported by application code.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationRunner {
    pub program: String,
    pub module: Option<String>,
}

/// Declared workspace inputs of one check. Directories include their members recursively.
/// External tools and toolchains must stay stable; unknown shell or extension effects block reuse.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationInputs {
    /// Workspace-relative files or directories the check reads.
    pub paths: Vec<String>,
    /// Environment variable names whose values the check reads.
    pub environment: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndependentReviewConfig {
    pub enabled: bool,
    pub min_changed_files: usize,
    /// Workspace-relative file or directory prefixes. Empty means only the file-count threshold.
    pub sensitive_paths: Vec<String>,
    pub max_rounds: u8,
}

impl Default for IndependentReviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_changed_files: 4,
            sensitive_paths: Vec::new(),
            max_rounds: 1,
        }
    }
}

/// Evidence needed for a declared behavior. Passing checks show coverage, not independent proof.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationRequirement {
    pub checks: Vec<String>,
    /// Require delivered browser pixels for the latest effect revision.
    pub visual: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCommand {
    pub name: String,
    pub command: String,
    /// File extensions without a dot that trigger this command; empty means any source edit.
    #[serde(default)]
    pub file_extensions: Vec<String>,
    #[serde(default = "default_verification_timeout")]
    pub timeout_secs: u64,
}

fn default_verification_timeout() -> u64 {
    0
}
