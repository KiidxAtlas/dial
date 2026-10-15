//! Turns a recipe into the exact commands that will run. Plans never include sudo and never
//! write outside Dial's install directory, except for Homebrew, which the user chooses.

use super::{Env, Method, Recipe, install_dir, locate};
use std::path::{Path, PathBuf};

/// One command in a plan. `program` is the resolved executable path, so the confirmation shows
/// exactly what will run and the step does not depend on the caller's `PATH`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Step {
    /// Shown to the user before anything runs.
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub id: &'static str,
    /// Human label, for example `npm into ~/.dial/tools/bun`.
    pub method: String,
    /// Where the install lands, shown in the confirmation. Relative for Homebrew.
    pub destination: PathBuf,
    pub steps: Vec<Step>,
    /// Where the installed program is expected once the steps succeed (empty: look on `PATH`).
    pub expected: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocked {
    /// Nothing automatic is possible; the hint tells the user what to do.
    Manual(String),
    /// Required package managers are not on `PATH`.
    NoPackageManager(Vec<&'static str>),
}

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manual(hint) => write!(f, "{hint}"),
            Self::NoPackageManager(tools) => write!(
                f,
                "needs {} on PATH; install one of them first, then retry",
                tools.join(" or ")
            ),
        }
    }
}

/// Plans the first method whose package manager is available, in the recipe's order.
/// Callers check `requires` themselves, because readiness depends on other recipes.
pub fn plan(recipe: &Recipe, env: &Env) -> Result<Plan, Blocked> {
    let destination = install_dir(&env.user_dir, recipe.id);
    let mut missing = Vec::new();
    for method in recipe.methods {
        match *method {
            Method::Manual(hint) => return Err(Blocked::Manual(hint.to_owned())),
            Method::Npm(packages) => match env.find("npm") {
                Some(npm) => return Ok(npm_plan(recipe, &destination, packages, &npm)),
                None => missing.push("npm"),
            },
            Method::Uv(package) => match env.find("uv") {
                Some(uv) => return Ok(uv_plan(recipe, &destination, package, &uv)),
                None => missing.push("uv"),
            },
            Method::Brew(formula) => match env.find("brew") {
                Some(brew) => return Ok(brew_plan(recipe, formula, &brew)),
                None => missing.push("brew"),
            },
        }
    }
    if missing.is_empty() {
        Err(Blocked::Manual("no install method for this system".into()))
    } else {
        missing.sort_unstable();
        missing.dedup();
        Err(Blocked::NoPackageManager(missing))
    }
}

fn program_text(path: &Path) -> String {
    path.display().to_string()
}

fn npm_plan(recipe: &Recipe, destination: &Path, packages: &[&str], npm: &Path) -> Plan {
    let mut args = vec![
        "install".to_owned(),
        "--prefix".to_owned(),
        program_text(destination),
        "--no-audit".to_owned(),
        "--no-fund".to_owned(),
    ];
    args.extend(packages.iter().map(|package| (*package).to_owned()));
    Plan {
        id: recipe.id,
        method: format!("npm into {}", destination.display()),
        destination: destination.to_path_buf(),
        steps: vec![Step {
            program: program_text(npm),
            args,
            env: Vec::new(),
        }],
        expected: destination.join(recipe.artifact),
    }
}

fn uv_plan(recipe: &Recipe, destination: &Path, package: &str, uv: &Path) -> Plan {
    Plan {
        id: recipe.id,
        method: format!("uv tool into {}", destination.display()),
        destination: destination.to_path_buf(),
        steps: vec![Step {
            program: program_text(uv),
            args: vec!["tool".into(), "install".into(), package.into()],
            env: vec![
                ("UV_TOOL_DIR".into(), program_text(&destination.join("uv"))),
                (
                    "UV_TOOL_BIN_DIR".into(),
                    program_text(&destination.join("bin")),
                ),
            ],
        }],
        expected: destination.join(recipe.artifact),
    }
}

fn brew_plan(recipe: &Recipe, formula: &str, brew: &Path) -> Plan {
    Plan {
        id: recipe.id,
        method: "Homebrew (no sudo; installs into Homebrew's prefix)".into(),
        destination: PathBuf::from("Homebrew prefix"),
        steps: vec![Step {
            program: program_text(brew),
            args: vec!["install".into(), formula.into()],
            env: Vec::new(),
        }],
        expected: PathBuf::new(),
    }
}

/// Required dependencies that are not ready yet.
pub fn unmet_requirements(recipe: &Recipe, env: &Env) -> Vec<&'static str> {
    recipe
        .requires
        .iter()
        .copied()
        .filter(|id| super::recipe(id).is_none_or(|required| locate(required, env).is_none()))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::setup::deps::recipe;

    /// Fake package managers: executable stubs on a fake `PATH` directory.
    pub(crate) fn fake_tools(root: &Path, tools: &[&str]) -> Env {
        let bin = root.join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in tools {
            let path = bin.join(tool);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        Env {
            path: vec![bin],
            user_dir: root.join("dial"),
        }
    }

    #[test]
    fn bun_prefers_npm_into_dial_dir_and_never_uses_sudo() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &["npm", "brew"]);
        let plan = plan(recipe("bun").unwrap(), &env).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert!(plan.steps[0].program.ends_with("/fakebin/npm"));
        assert!(plan.steps[0].args.contains(&"bun".to_owned()));
        assert!(plan.destination.starts_with(env.user_dir.join("tools")));
        assert!(
            plan.steps
                .iter()
                .all(|step| !step.program.ends_with("sudo"))
        );
    }

    #[test]
    fn falls_back_to_brew_when_npm_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &["brew"]);
        let plan = plan(recipe("bun").unwrap(), &env).unwrap();
        assert!(plan.steps[0].display().ends_with("brew install bun"));
    }

    #[test]
    fn uv_tools_install_under_dial_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &["uv"]);
        let plan = plan(recipe("mlx-whisper").unwrap(), &env).unwrap();
        let step = &plan.steps[0];
        assert_eq!(step.args, vec!["tool", "install", "mlx-whisper"]);
        assert!(
            step.env
                .iter()
                .all(|(_, value)| value.starts_with(&env.user_dir.display().to_string()))
        );
    }

    #[test]
    fn no_package_manager_is_reported_plainly() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &[]);
        assert_eq!(
            plan(recipe("bun").unwrap(), &env),
            Err(Blocked::NoPackageManager(vec!["brew", "npm"]))
        );
    }

    #[test]
    fn manual_recipes_return_their_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &["npm"]);
        match plan(recipe("hindsight").unwrap(), &env) {
            Err(Blocked::Manual(hint)) => assert!(hint.contains("Docker")),
            other => panic!("expected manual hint, got {other:?}"),
        }
    }
}
