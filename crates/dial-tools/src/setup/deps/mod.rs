//! Registry of the external software that optional Dial features depend on.
//!
//! Each [`Recipe`] says how to detect a dependency, which install methods exist, and which
//! settings to save once the install succeeds and the health check passes. Detection and
//! planning are pure over an [`Env`], so tests can fake the package managers on `PATH`.

pub mod plan;
pub mod prebuilt;
pub mod run;
pub mod updates;

use std::path::{Path, PathBuf};

/// Minimum Bun version that the `bun` recipe accepts.
pub const BUN_MIN: (u64, u64, u64) = (1, 3, 14);

/// Minimum Node.js major.minor.patch for the `node` recipe (`eval.node` needs Node 22+).
pub const NODE_MIN: (u64, u64, u64) = (22, 0, 0);

/// One way to obtain a dependency. Every method installs only into Dial's user directory,
/// or uses an existing package manager without elevated privileges. Nothing runs with sudo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// npm packages installed with `npm install --prefix <install dir>`.
    Npm(&'static [&'static str]),
    /// A Python tool installed with `uv tool install` into the install dir.
    Uv(&'static str),
    /// A Homebrew formula. Homebrew installs into its own prefix, without sudo.
    Brew(&'static str),
    /// Needs the user: a service, a model file, or a privileged install. Dial shows the hint.
    Manual(&'static str),
    /// A pinned release archive from the project's own download, verified by SHA-256 and unpacked
    /// into Dial's install directory. Used only when no package manager above is available.
    Download(prebuilt::Prebuilt),
    /// A Go program installed with `go install <package>` into the install dir's `bin`.
    Go(&'static str),
    /// A rustup component (`rustup component add <name>`), which lands in the Rust toolchain.
    Rustup(&'static str),
}

/// Where the artifact a recipe detects lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    /// `<user_dir>/tools/<id>/…`: Dial's own install directory.
    Install,
    /// `<user_dir>/…`: shared Dial state, such as the MLX runtime.
    UserDir,
}

#[derive(Clone, Copy, Debug)]
pub struct Recipe {
    pub id: &'static str,
    pub title: &'static str,
    /// Recipe ids that must be ready first.
    pub requires: &'static [&'static str],
    /// Program searched for on `PATH` (empty: none).
    pub program: &'static str,
    pub base: Base,
    /// Artifact relative to `base` that proves a Dial install exists (empty: none).
    pub artifact: &'static str,
    /// Install methods in order of preference.
    pub methods: &'static [Method],
}

const LSP_TS: &[&str] = &["typescript", "typescript-language-server"];
const LSP_VSCODE: &[&str] = &["vscode-langservers-extracted"];

/// A language server installed from npm; the recipe id is the program name.
macro_rules! npm_lsp {
    ($program:literal, $title:literal, $packages:expr) => {
        Recipe {
            id: $program,
            title: $title,
            requires: &["node"],
            program: $program,
            base: Base::Install,
            artifact: concat!("node_modules/.bin/", $program),
            methods: &[Method::Npm($packages)],
        }
    };
}

/// A language server installed as a Python tool with uv.
macro_rules! uv_lsp {
    ($program:literal, $title:literal, $package:literal) => {
        Recipe {
            id: $program,
            title: $title,
            requires: &["uv"],
            program: $program,
            base: Base::Install,
            artifact: concat!("bin/", $program),
            methods: &[Method::Uv($package)],
        }
    };
}

/// A language server from a Homebrew formula.
macro_rules! brew_lsp {
    ($program:literal, $title:literal, $formula:literal) => {
        Recipe {
            id: $program,
            title: $title,
            requires: &[],
            program: $program,
            base: Base::Install,
            artifact: "",
            methods: &[Method::Brew($formula)],
        }
    };
}

/// The llama.cpp server binary on this platform (GGUF models run on it).
const LLAMA_SERVER: &str = if cfg!(windows) {
    "llama-server.exe"
} else {
    "llama-server"
};

pub const RECIPES: &[Recipe] = &[
    Recipe {
        id: "bun",
        title: "Bun",
        requires: &[],
        program: "bun",
        base: Base::Install,
        artifact: "node_modules/.bin/bun",
        methods: &[
            Method::Npm(&["bun"]),
            Method::Brew("bun"),
            Method::Download(prebuilt::BUN),
        ],
    },
    Recipe {
        id: "hindsight",
        title: "Hindsight service",
        requires: &[],
        program: "",
        base: Base::Install,
        artifact: "",
        methods: &[Method::Manual(
            "run ghcr.io/vectorize-io/hindsight with Docker, then set memory.hindsight.endpoint",
        )],
    },
    Recipe {
        id: "mlx-runtime",
        title: "Local MLX runtime",
        requires: &[],
        program: "",
        base: Base::UserDir,
        artifact: "python/.venv/bin/python",
        methods: &[Method::Manual(
            "run `dial setup -m` (needs uv and Apple Silicon)",
        )],
    },
    Recipe {
        id: "espeak-ng",
        title: "espeak-ng",
        requires: &[],
        program: "espeak-ng",
        base: Base::Install,
        artifact: "",
        methods: &[Method::Brew("espeak-ng")],
    },
    Recipe {
        id: "piper",
        title: "Piper",
        requires: &["uv"],
        program: "piper",
        base: Base::Install,
        artifact: "bin/piper",
        methods: &[
            Method::Uv("piper-tts"),
            Method::Manual("pip install piper-tts"),
        ],
    },
    Recipe {
        id: "whisper-cpp",
        title: "whisper.cpp",
        requires: &[],
        program: "whisper-cli",
        base: Base::Install,
        artifact: "",
        methods: &[Method::Brew("whisper-cpp")],
    },
    Recipe {
        id: "mlx-whisper",
        title: "mlx-whisper",
        requires: &["uv"],
        program: "mlx_whisper",
        base: Base::Install,
        artifact: "bin/mlx_whisper",
        methods: &[Method::Uv("mlx-whisper")],
    },
    Recipe {
        id: "rust-analyzer",
        title: "rust-analyzer",
        requires: &[],
        program: "rust-analyzer",
        base: Base::Install,
        artifact: "",
        methods: &[
            Method::Rustup("rust-analyzer"),
            Method::Brew("rust-analyzer"),
        ],
    },
    Recipe {
        id: "typescript-language-server",
        title: "typescript-language-server",
        requires: &["node"],
        program: "typescript-language-server",
        base: Base::Install,
        artifact: "node_modules/.bin/typescript-language-server",
        methods: &[Method::Npm(LSP_TS)],
    },
    Recipe {
        id: "pyright-langserver",
        title: "pyright",
        requires: &["node"],
        program: "pyright-langserver",
        base: Base::Install,
        artifact: "node_modules/.bin/pyright-langserver",
        methods: &[Method::Npm(&["pyright"])],
    },
    Recipe {
        id: "gopls",
        title: "gopls",
        requires: &[],
        program: "gopls",
        base: Base::Install,
        artifact: "",
        methods: &[
            Method::Go("golang.org/x/tools/gopls@latest"),
            Method::Brew("gopls"),
        ],
    },
    Recipe {
        id: "lua-language-server",
        title: "lua-language-server",
        requires: &[],
        program: "lua-language-server",
        base: Base::Install,
        artifact: "",
        methods: &[Method::Brew("lua-language-server")],
    },
    Recipe {
        id: "zls",
        title: "zls",
        requires: &[],
        program: "zls",
        base: Base::Install,
        artifact: "",
        methods: &[Method::Brew("zls")],
    },
    npm_lsp!(
        "bash-language-server",
        "bash-language-server",
        &["bash-language-server"]
    ),
    npm_lsp!(
        "yaml-language-server",
        "yaml-language-server",
        &["yaml-language-server"]
    ),
    npm_lsp!(
        "vscode-html-language-server",
        "HTML language server",
        LSP_VSCODE
    ),
    npm_lsp!(
        "vscode-css-language-server",
        "CSS language server",
        LSP_VSCODE
    ),
    npm_lsp!(
        "vscode-json-language-server",
        "JSON language server",
        LSP_VSCODE
    ),
    npm_lsp!(
        "vscode-eslint-language-server",
        "ESLint language server",
        LSP_VSCODE
    ),
    npm_lsp!(
        "svelteserver",
        "Svelte language server",
        &["svelte-language-server"]
    ),
    npm_lsp!(
        "vue-language-server",
        "Vue language server",
        &["@vue/language-server"]
    ),
    npm_lsp!(
        "astro-ls",
        "Astro language server",
        &["@astrojs/language-server"]
    ),
    npm_lsp!("intelephense", "Intelephense", &["intelephense"]),
    npm_lsp!(
        "docker-langserver",
        "Dockerfile language server",
        &["dockerfile-language-server-nodejs"]
    ),
    npm_lsp!(
        "prisma-language-server",
        "Prisma language server",
        &["@prisma/language-server"]
    ),
    npm_lsp!(
        "vim-language-server",
        "Vim language server",
        &["vim-language-server"]
    ),
    npm_lsp!(
        "graphql-lsp",
        "GraphQL language server",
        &["graphql-language-service-cli"]
    ),
    npm_lsp!(
        "tailwindcss-language-server",
        "Tailwind CSS language server",
        &["@tailwindcss/language-server"]
    ),
    npm_lsp!(
        "emmet-language-server",
        "Emmet language server",
        &["emmet-language-server"]
    ),
    npm_lsp!("biome", "Biome", &["@biomejs/biome"]),
    uv_lsp!("pylsp", "python-lsp-server", "python-lsp-server"),
    uv_lsp!("basedpyright-langserver", "basedpyright", "basedpyright"),
    uv_lsp!("ruff", "Ruff", "ruff"),
    uv_lsp!("ty", "ty", "ty"),
    brew_lsp!("marksman", "marksman", "marksman"),
    brew_lsp!("texlab", "texlab", "texlab"),
    brew_lsp!("jdtls", "Eclipse JDT language server", "jdtls"),
    brew_lsp!("elixir-ls", "ElixirLS", "elixir-ls"),
    brew_lsp!("gleam", "Gleam", "gleam"),
    brew_lsp!("helm_ls", "helm-ls", "helm-ls"),
    brew_lsp!("nixd", "nixd", "nixd"),
    Recipe {
        id: "node",
        title: "Node.js",
        requires: &[],
        program: "node",
        base: Base::Install,
        artifact: "",
        methods: &[Method::Brew("node"), Method::Download(prebuilt::NODE)],
    },
    Recipe {
        id: "uv",
        title: "uv",
        requires: &[],
        program: "uv",
        base: Base::Install,
        artifact: "",
        methods: &[Method::Brew("uv"), Method::Download(prebuilt::UV)],
    },
    Recipe {
        id: "ollama",
        title: "Ollama",
        requires: &[],
        program: "ollama",
        base: Base::Install,
        artifact: "",
        // Homebrew first; otherwise the official installer page is the only hint Dial can give.
        methods: &[
            Method::Brew("ollama"),
            Method::Manual("install Ollama from https://ollama.com/download"),
        ],
    },
    Recipe {
        id: "llama-cpp",
        title: "llama.cpp (llama-server)",
        requires: &[],
        program: LLAMA_SERVER,
        base: Base::Install,
        artifact: LLAMA_SERVER,
        // Homebrew where it exists; otherwise the pinned release archive (Windows and Linux x64).
        methods: &[
            Method::Brew("llama.cpp"),
            Method::Download(prebuilt::LLAMA_CPP),
            Method::Manual(
                "install llama.cpp from https://github.com/ggml-org/llama.cpp/releases and set DIAL_LLAMA_SERVER to its llama-server",
            ),
        ],
    },
];

pub fn recipe(id: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|recipe| recipe.id == id)
}

/// Dial's own install directory for one dependency.
pub fn install_dir(user_dir: &Path, id: &str) -> PathBuf {
    user_dir.join("tools").join(id)
}

/// What `PATH` and the user directory look like at one moment.
#[derive(Clone, Debug)]
pub struct Env {
    pub path: Vec<PathBuf>,
    pub user_dir: PathBuf,
}

impl Env {
    pub fn current(user_dir: &Path) -> Self {
        let path = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        Self {
            path,
            user_dir: user_dir.to_path_buf(),
        }
    }

    pub fn find(&self, program: &str) -> Option<PathBuf> {
        if program.is_empty() {
            return None;
        }
        self.path
            .iter()
            .map(|dir| dir.join(program))
            .find(|candidate| is_executable(candidate))
    }
}

pub fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Where a recipe's artifact lives under `env`: a Dial install first, then `PATH`.
pub fn locate(recipe: &Recipe, env: &Env) -> Option<PathBuf> {
    installed(recipe, env).or_else(|| env.find(recipe.program))
}

/// What Dial itself installed for `recipe`: its artifact, or the binary of a direct download.
fn installed(recipe: &Recipe, env: &Env) -> Option<PathBuf> {
    let dir = install_dir(&env.user_dir, recipe.id);
    if !recipe.artifact.is_empty() {
        let base = match recipe.base {
            Base::Install => dir.clone(),
            Base::UserDir => env.user_dir.clone(),
        };
        let path = base.join(recipe.artifact);
        if path.exists() {
            return Some(path);
        }
    }
    recipe
        .methods
        .iter()
        .find_map(|method| match method {
            Method::Download(prebuilt) => Some(dir.join(prebuilt.binary)),
            _ => None,
        })
        .filter(|path| path.exists())
}

/// The default whisper.cpp model (`ggml-base.en.bin`) from whisper.cpp's own download source.
/// The Hugging Face LFS etag publishes its SHA-256; the download is refused unless it matches.
pub const WHISPER_MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin";
pub const WHISPER_MODEL_SHA256: &str =
    "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002";
pub const WHISPER_MODEL_BYTES: u64 = 147_964_211;

/// Where Dial keeps the whisper.cpp model it downloads during setup.
pub fn whisper_model_path(user_dir: &Path) -> PathBuf {
    user_dir
        .join("models")
        .join("whisper")
        .join("ggml-base.en.bin")
}

/// Installed and usable. whisper.cpp also needs its model file, so a program alone is not enough.
pub fn is_ready(recipe: &Recipe, env: &Env) -> bool {
    locate(recipe, env).is_some()
        && (recipe.id != "whisper-cpp" || whisper_model_path(&env.user_dir).is_file())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ready,
    Missing,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub id: &'static str,
    pub title: &'static str,
    pub status: Status,
    pub detail: String,
}

/// Detection for `dial doctor` and the `/setup` list.
pub fn report(env: &Env) -> Vec<Row> {
    RECIPES
        .iter()
        .map(
            |recipe| match locate(recipe, env).filter(|_| is_ready(recipe, env)) {
                Some(path) => Row {
                    id: recipe.id,
                    title: recipe.title,
                    status: Status::Ready,
                    detail: path.display().to_string(),
                },
                None => Row {
                    id: recipe.id,
                    title: recipe.title,
                    status: Status::Missing,
                    detail: recipe
                        .methods
                        .iter()
                        .find_map(|method| match method {
                            Method::Manual(hint) => Some((*hint).to_owned()),
                            _ => None,
                        })
                        .unwrap_or_else(|| "not installed".to_owned()),
                },
            },
        )
        .collect()
}

/// Settings written after a dependency is installed and healthy. Empty for prerequisites.
pub fn settings_for(id: &str, location: &Path) -> Vec<(String, String)> {
    let value = location.display().to_string();
    let set = |path: &str| vec![(path.to_owned(), value.clone())];
    match id {
        "espeak-ng" => set("voice.tts.espeak_program"),
        "piper" => set("voice.tts.piper_program"),
        "whisper-cpp" => set("voice.stt.whisper_cpp_program"),
        "node" => set("eval.node"),
        "mlx-whisper" => set("voice.stt.mlx_whisper_program"),
        "rust-analyzer" => lsp("rust-analyzer", &value),
        "typescript-language-server" => lsp("typescript-language-server", &value),
        "pyright-langserver" => lsp("pyright", &value),
        "gopls" => lsp("gopls", &value),
        "lua-language-server" => lsp("lua-language-server", &value),
        "zls" => lsp("zls", &value),
        _ => Vec::new(),
    }
}

fn lsp(name: &str, value: &str) -> Vec<(String, String)> {
    vec![(format!("lsp.{name}.program"), value.to_owned())]
}

#[cfg(test)]
mod tests;
