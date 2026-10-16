//! Turns a recipe into the exact commands that will run. Plans never include sudo and never
//! write outside Dial's install directory, except for Homebrew, which the user chooses.

use super::prebuilt::Asset;
use super::{Env, Method, Recipe, install_dir, locate, recipe};
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

/// A verified archive download, run before any step. Nothing is fetched without the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch {
    pub asset: Asset,
    /// Install directory the archive is unpacked into.
    pub dest: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub id: &'static str,
    /// Human label, for example `npm into ~/.dial/tools/bun`.
    pub method: String,
    /// Where the install lands, shown in the confirmation. Relative for Homebrew.
    pub destination: PathBuf,
    /// A direct download, run before `steps`.
    pub fetch: Option<Fetch>,
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

/// Plans the first method whose package manager is available, in the recipe's order. A direct
/// download is the last resort, and only on platforms that have a pinned archive.
/// Callers check `requires` themselves, because readiness depends on other recipes.
pub fn plan(recipe: &Recipe, env: &Env) -> Result<Plan, Blocked> {
    let destination = install_dir(&env.user_dir, recipe.id);
    let mut missing = Vec::new();
    for method in recipe.methods {
        match *method {
            Method::Manual(hint) => return Err(Blocked::Manual(hint.to_owned())),
            Method::Npm(packages) => match find_npm(env) {
                Some(npm) => return Ok(npm_plan(recipe, env, &destination, packages, &npm)),
                None => missing.push("npm"),
            },
            Method::Uv(package) => match find_uv(env) {
                Some(uv) => return Ok(uv_plan(recipe, &destination, package, &uv)),
                None => missing.push("uv"),
            },
            Method::Brew(formula) => match env.find("brew") {
                Some(brew) => return Ok(brew_plan(recipe, formula, &brew)),
                None => missing.push("brew"),
            },
            Method::Go(package) => match env.find("go") {
                Some(go) => return Ok(go_plan(recipe, &destination, package, &go)),
                None => missing.push("go"),
            },
            Method::Rustup(component) => match env.find("rustup") {
                Some(rustup) => return Ok(rustup_plan(recipe, component, &rustup)),
                None => missing.push("rustup"),
            },
            Method::Download(prebuilt) => {
                if let Some(asset) = super::prebuilt::current_platform()
                    .and_then(|platform| prebuilt.asset(platform))
                {
                    return Ok(download_plan(recipe, &destination, prebuilt.version, asset));
                }
            }
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

/// `npm` on `PATH`, or the one that comes with a Node.js Dial installed.
fn find_npm(env: &Env) -> Option<PathBuf> {
    env.find("npm").or_else(|| {
        let node = locate(recipe("node")?, env)?;
        let npm = node.parent()?.join("npm");
        super::is_executable(&npm).then_some(npm)
    })
}

/// `uv` on `PATH`, or the one Dial installed.
fn find_uv(env: &Env) -> Option<PathBuf> {
    locate(recipe("uv")?, env)
}

/// A `PATH` that starts with the directory of `program`, so scripts such as npm find the node
/// beside them even when that node is not on the user's `PATH`.
fn path_with(program: &Path, env: &Env) -> String {
    let dirs = program
        .parent()
        .into_iter()
        .map(Path::to_path_buf)
        .chain(env.path.iter().cloned());
    std::env::join_paths(dirs)
        .map(|joined| joined.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn npm_plan(recipe: &Recipe, env: &Env, destination: &Path, packages: &[&str], npm: &Path) -> Plan {
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
        fetch: None,
        steps: vec![Step {
            program: program_text(npm),
            args,
            env: vec![("PATH".into(), path_with(npm, env))],
        }],
        expected: destination.join(recipe.artifact),
    }
}

fn uv_plan(recipe: &Recipe, destination: &Path, package: &str, uv: &Path) -> Plan {
    Plan {
        id: recipe.id,
        method: format!("uv tool into {}", destination.display()),
        destination: destination.to_path_buf(),
        fetch: None,
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
        fetch: None,
        steps: vec![Step {
            program: program_text(brew),
            args: vec!["install".into(), formula.into()],
            env: Vec::new(),
        }],
        expected: PathBuf::new(),
    }
}

fn go_plan(recipe: &Recipe, destination: &Path, package: &str, go: &Path) -> Plan {
    let bin = destination.join("bin");
    Plan {
        id: recipe.id,
        method: format!("go install into {}", bin.display()),
        destination: destination.to_path_buf(),
        fetch: None,
        steps: vec![Step {
            program: program_text(go),
            args: vec!["install".into(), package.into()],
            env: vec![("GOBIN".into(), program_text(&bin))],
        }],
        expected: bin.join(recipe.program),
    }
}

fn rustup_plan(recipe: &Recipe, component: &str, rustup: &Path) -> Plan {
    Plan {
        id: recipe.id,
        method: "rustup component (installs into the Rust toolchain, no sudo)".into(),
        destination: PathBuf::from("Rust toolchain"),
        fetch: None,
        steps: vec![Step {
            program: program_text(rustup),
            args: vec!["component".into(), "add".into(), component.into()],
            env: Vec::new(),
        }],
        expected: PathBuf::new(),
    }
}

fn download_plan(
    recipe: &Recipe,
    destination: &Path,
    version: &str,
    asset: &'static Asset,
) -> Plan {
    Plan {
        id: recipe.id,
        method: format!(
            "download {} {version} into {} (sha256 checked)",
            recipe.title,
            destination.display()
        ),
        destination: destination.to_path_buf(),
        fetch: Some(Fetch {
            asset: *asset,
            dest: destination.to_path_buf(),
        }),
        steps: Vec::new(),
        expected: destination.join(download_binary(recipe)),
    }
}

fn download_binary(recipe: &Recipe) -> &'static str {
    recipe
        .methods
        .iter()
        .find_map(|method| match method {
            Method::Download(prebuilt) => Some(prebuilt.binary),
            _ => None,
        })
        .unwrap_or("")
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
    use crate::setup::deps::{prebuilt, recipe};

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
        assert!(plan.fetch.is_none());
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
        // Bun can be downloaded on supported platforms, so only the package managers that
        // remain are named when nothing is on PATH.
        let blocked = plan(recipe("gopls").unwrap(), &env);
        assert_eq!(blocked, Err(Blocked::NoPackageManager(vec!["brew", "go"])));
        if prebuilt::current_platform().is_some() {
            assert!(plan(recipe("bun").unwrap(), &env).is_ok());
        }
    }

    #[test]
    fn go_and_rustup_recipes_plan_into_the_expected_places() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &["go", "rustup"]);
        let gopls = plan(recipe("gopls").unwrap(), &env).unwrap();
        assert_eq!(gopls.steps[0].args[0], "install");
        assert_eq!(
            gopls.steps[0].env,
            vec![(
                "GOBIN".to_owned(),
                env.user_dir.join("tools/gopls/bin").display().to_string()
            )]
        );
        assert_eq!(gopls.expected, env.user_dir.join("tools/gopls/bin/gopls"));
        let ra = plan(recipe("rust-analyzer").unwrap(), &env).unwrap();
        assert_eq!(ra.steps[0].args, vec!["component", "add", "rust-analyzer"]);
        assert!(ra.expected.as_os_str().is_empty());
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

    #[test]
    fn node_downloads_into_dial_dir_when_no_package_manager_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &[]);
        if prebuilt::current_platform().is_none() {
            return;
        }
        let plan = plan(recipe("node").unwrap(), &env).unwrap();
        let fetch = plan.fetch.expect("a verified download");
        assert_eq!(fetch.dest, env.user_dir.join("tools").join("node"));
        assert!(fetch.asset.url.starts_with("https://nodejs.org/dist/"));
        assert_eq!(plan.expected, fetch.dest.join("bin/node"));
        assert!(plan.steps.is_empty());
        assert!(plan.method.contains("sha256 checked"));
    }

    #[test]
    fn npm_recipes_use_the_npm_of_a_dial_installed_node() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &[]);
        let bin = env.user_dir.join("tools/node/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("node"), "#!/bin/sh\necho v22.23.3\n").unwrap();
        std::fs::write(bin.join("npm"), "#!/bin/sh\nexit 0\n").unwrap();
        for path in [bin.join("node"), bin.join("npm")] {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let plan = plan(recipe("pyright-langserver").unwrap(), &env).unwrap();
        let step = &plan.steps[0];
        assert_eq!(step.program, bin.join("npm").display().to_string());
        let path = step.env.iter().find(|(key, _)| key == "PATH").unwrap();
        assert!(path.1.starts_with(&bin.display().to_string()));
    }
}
