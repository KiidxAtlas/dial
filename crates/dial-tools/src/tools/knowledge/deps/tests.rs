use super::*;
use std::path::Path;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn dev(ws: Workspace, deny_read: Vec<PathBuf>) -> Workspace {
    ws.with_policy(Policy::Dev {
        deny_read,
        extra_write: Vec::new(),
        allow_network: false,
    })
}

const LOCK: &str = r#"
version = 4

[[package]]
name = "demo-crate"
version = "1.2.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "demo-crate"
version = "0.9.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "local"
version = "0.1.0"
"#;

/// A project with a Cargo.lock and a cargo home holding only demo-crate 1.2.0.
fn cargo_fixture() -> (tempfile::TempDir, tempfile::TempDir, Caches) {
    let project = tempfile::tempdir().unwrap();
    write(&project.path().join("Cargo.lock"), LOCK);
    let home = tempfile::tempdir().unwrap();
    let crate_dir = home
        .path()
        .join("registry/src/index.crates.io-0123456789abcdef/demo-crate-1.2.0");
    write(
        &crate_dir.join("Cargo.toml"),
        "[package]\nname = \"demo-crate\"\n",
    );
    write(
        &crate_dir.join("src/lib.rs"),
        "pub fn needle() {}\npub fn other() {}\n",
    );
    let caches = Caches {
        cargo_home: Some(home.path().to_path_buf()),
        go_mod_cache: None,
        ..Default::default()
    };
    (project, home, caches)
}

#[test]
fn cargo_resolves_the_locked_registry_source() {
    let (project, _home, caches) = cargo_fixture();
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    let out = run(&ws, &json!({"name": "demo_crate"}), &caches).unwrap();
    assert!(
        out.starts_with("cargo demo-crate 1.2.0 (resolved from Cargo.lock, registry+"),
        "{out}"
    );
    assert!(out.contains("other locked versions: 0.9.0"), "{out}");
    assert!(out.contains("  src/\n"), "{out}");

    let out = run(
        &ws,
        &json!({"name": "demo-crate", "query": "needle"}),
        &caches,
    )
    .unwrap();
    assert!(out.contains("src/lib.rs:1: pub fn needle() {}"), "{out}");
    assert!(!out.contains("other()"), "{out}");

    let out = run(
        &ws,
        &json!({"name": "demo-crate", "path": "src/lib.rs:2-2"}),
        &caches,
    )
    .unwrap();
    assert!(
        out.contains("2:pub fn other() {}\n[src/lib.rs lines 2-2 of 2]"),
        "{out}"
    );

    // Locked but not downloaded: reported, never fetched.
    let error = run(&ws, &json!({"name": "demo-crate@0.9.0"}), &caches).unwrap_err();
    assert!(
        error.contains("is locked but its source is not in"),
        "{error}"
    );
    let error = run(
        &ws,
        &json!({"name": "local", "ecosystem": "cargo"}),
        &caches,
    )
    .unwrap_err();
    assert!(error.contains("path dependency"), "{error}");
    let error = run(&ws, &json!({"name": "absent"}), &caches).unwrap_err();
    assert!(error.contains("absent is not in Cargo.lock"), "{error}");
}

#[test]
fn package_paths_never_leave_the_package_root() {
    let (project, home, caches) = cargo_fixture();
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    for path in ["../../../../../etc/passwd", "/etc/passwd", "src/../../x"] {
        let error = run(&ws, &json!({"name": "demo-crate", "path": path}), &caches).unwrap_err();
        assert!(error.contains("without `..`"), "{path}: {error}");
    }
    let outside = tempfile::tempdir().unwrap();
    write(&outside.path().join("secret"), "token\n");
    let crate_dir = home
        .path()
        .join("registry/src/index.crates.io-0123456789abcdef/demo-crate-1.2.0");
    std::os::unix::fs::symlink(outside.path(), crate_dir.join("escape")).unwrap();
    let error = run(
        &ws,
        &json!({"name": "demo-crate", "path": "escape/secret"}),
        &caches,
    )
    .unwrap_err();
    assert!(error.contains("escapes the package root"), "{error}");
    // Searches do not follow the link either.
    let out = run(
        &ws,
        &json!({"name": "demo-crate", "query": "token"}),
        &caches,
    )
    .unwrap();
    assert!(out.contains("no matches"), "{out}");
}

#[test]
fn caches_outside_the_workspace_follow_the_sandbox_read_policy() {
    let (project, home, caches) = cargo_fixture();
    // Strict (also the default): the resolved registry cache is granted to this tool alone.
    let strict = Workspace::new(project.path()).unwrap();
    let out = run(&strict, &json!({"name": "demo-crate"}), &caches).unwrap();
    assert!(out.contains("1.2.0"), "{out}");
    // A denied path still wins under both policies.
    for policy in [
        Policy::Strict {
            deny_read: vec![home.path().to_path_buf()],
            allow_network: false,
            extra_write: Vec::new(),
        },
        Policy::Dev {
            deny_read: vec![home.path().to_path_buf()],
            extra_write: Vec::new(),
            allow_network: false,
        },
    ] {
        let denied = Workspace::new(project.path()).unwrap().with_policy(policy);
        let error = run(&denied, &json!({"name": "demo-crate"}), &caches).unwrap_err();
        assert!(error.contains("denies reading"), "{error}");
    }
    // Strict grants only the configured dependency caches, not the rest of the home.
    let other = Caches {
        cargo_home: Some(project.path().join("no-cargo-home")),
        go_mod_cache: None,
        ..Default::default()
    };
    assert!(!other.contain(&fs::canonicalize(home.path()).unwrap()));
    assert!(caches.contain(&fs::canonicalize(home.path().join("registry/src")).unwrap()));
    assert!(!caches.contain(&fs::canonicalize(home.path()).unwrap()));
    // An attached root grants the read as well.
    let added = Workspace::new(project.path())
        .unwrap()
        .with_additional_roots(&[home.path().to_path_buf()])
        .unwrap();
    assert!(run(&added, &json!({"name": "demo-crate"}), &caches).is_ok());
}

#[test]
fn the_strict_grant_does_not_cover_home_paths_outside_the_caches() {
    // The cargo home resolves a registry source, but the only package directory found is
    // reached through a symlink to elsewhere in the home: refused before any grant applies.
    let (project, home, caches) = cargo_fixture();
    let index = home
        .path()
        .join("registry/src/index.crates.io-0123456789abcdef");
    let outside = tempfile::tempdir().unwrap();
    write(
        &outside.path().join("demo-crate-1.2.0/Cargo.toml"),
        "[package]\n",
    );
    fs::remove_dir_all(index.join("demo-crate-1.2.0")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("demo-crate-1.2.0"),
        index.join("demo-crate-1.2.0"),
    )
    .unwrap();
    let strict = Workspace::new(project.path()).unwrap();
    let error = run(&strict, &json!({"name": "demo-crate"}), &caches).unwrap_err();
    assert!(error.contains("refusing to follow"), "{error}");
}

#[test]
fn npm_resolves_installed_packages_inside_the_workspace() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    write(&root.join("package.json"), "{\"dependencies\": {}}");
    write(
        &root.join("node_modules/left-pad/package.json"),
        "{\"name\": \"left-pad\", \"version\": \"1.3.0\"}",
    );
    write(
        &root.join("node_modules/left-pad/index.js"),
        "module.exports = leftPad;\n",
    );
    write(
        &root.join("node_modules/@scope/pkg/package.json"),
        "{\"version\": \"2.0.0\"}",
    );
    // Inside the workspace, so the default strict policy reads it.
    let ws = Workspace::new(root).unwrap();
    let caches = Caches::default();
    let out = run(
        &ws,
        &json!({"name": "left-pad", "query": "leftPad"}),
        &caches,
    )
    .unwrap();
    assert!(
        out.starts_with("npm left-pad 1.3.0 (resolved from node_modules/left-pad/package.json)"),
        "{out}"
    );
    assert!(
        out.contains("index.js:1: module.exports = leftPad;"),
        "{out}"
    );
    let out = run(
        &ws,
        &json!({"name": "@scope/pkg", "ecosystem": "npm"}),
        &caches,
    )
    .unwrap();
    assert!(out.starts_with("npm @scope/pkg 2.0.0"), "{out}");
    let error = run(&ws, &json!({"name": "left-pad@9.9.9"}), &caches).unwrap_err();
    assert!(error.contains("node_modules has 1.3.0"), "{error}");

    for name in ["../outside", "a/../../b", "@scope/../x", ""] {
        let error = run(&ws, &json!({"name": name, "ecosystem": "npm"}), &caches).unwrap_err();
        assert!(error.contains("invalid package name"), "{name}: {error}");
    }
    // A linked package pointing out of node_modules is refused rather than followed.
    let outside = tempfile::tempdir().unwrap();
    write(
        &outside.path().join("package.json"),
        "{\"version\": \"0.0.1\"}",
    );
    std::os::unix::fs::symlink(outside.path(), root.join("node_modules/linked")).unwrap();
    let error = run(&ws, &json!({"name": "linked", "ecosystem": "npm"}), &caches).unwrap_err();
    assert!(error.contains("refusing to follow it"), "{error}");
}

#[test]
fn python_resolves_distributions_and_import_names_in_the_project_venv() {
    let project = tempfile::tempdir().unwrap();
    let site = project.path().join(".venv/lib/python3.12/site-packages");
    write(
        &site.join("PyYAML-6.0.1.dist-info/top_level.txt"),
        "_yaml\nyaml\n",
    );
    write(
        &site.join("yaml/__init__.py"),
        "def safe_load(stream): ...\n",
    );
    write(
        &project.path().join("pyproject.toml"),
        "[project]\nname = \"x\"\n",
    );
    let ws = Workspace::new(project.path()).unwrap();
    let caches = Caches::default();
    for name in ["pyyaml", "yaml"] {
        let out = run(&ws, &json!({"name": name, "query": "safe_load"}), &caches).unwrap();
        assert!(out.starts_with("python PyYAML 6.0.1"), "{name}: {out}");
        assert!(out.contains("__init__.py:1: def safe_load"), "{out}");
    }
}

#[test]
fn go_resolves_required_modules_from_the_module_cache() {
    let project = tempfile::tempdir().unwrap();
    write(
        &project.path().join("go.mod"),
        "module example.com/app\n\ngo 1.22\n\nrequire (\n\tgithub.com/Foo/bar v1.2.3 // indirect\n)\n",
    );
    let cache = tempfile::tempdir().unwrap();
    write(
        &cache.path().join("github.com/!foo/bar@v1.2.3/bar.go"),
        "package bar\n",
    );
    let caches = Caches {
        cargo_home: None,
        go_mod_cache: Some(cache.path().to_path_buf()),
        ..Default::default()
    };
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    let out = run(&ws, &json!({"name": "bar"}), &caches).unwrap();
    assert!(
        out.starts_with("go github.com/Foo/bar v1.2.3 (resolved from go.mod)"),
        "{out}"
    );
    assert!(out.contains("  bar.go\n"), "{out}");
}

#[test]
fn schema_is_read_only_and_needs_no_approval() {
    let tool = DepSource;
    assert!(!tool.needs_approval(&json!({"name": "x"})));
    assert!(tool.read_only_hint());
    assert_eq!(tool.concurrency(&json!({})), Concurrency::Shared);
    assert_eq!(tool.schema()["function"]["name"], "dep_source");
}

#[test]
fn composer_reads_the_locked_package_from_vendor() {
    let project = tempfile::tempdir().unwrap();
    write(
        &project.path().join("composer.lock"),
        r#"{"packages":[{"name":"monolog/monolog","version":"3.5.0"}],"packages-dev":[{"name":"phpunit/phpunit","version":"v10.5.2"}]}"#,
    );
    write(
        &project.path().join("vendor/monolog/monolog/src/Logger.php"),
        "<?php\nclass Logger { public function needle() {} }\n",
    );
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    let caches = Caches::default();
    let out = run(
        &ws,
        &json!({"name": "Monolog/Monolog", "query": "needle"}),
        &caches,
    )
    .unwrap();
    assert!(
        out.starts_with("composer monolog/monolog 3.5.0 (resolved from composer.lock, vendor/)"),
        "{out}"
    );
    assert!(out.contains("src/Logger.php:2: class Logger"), "{out}");
    let error = run(&ws, &json!({"name": "monolog/monolog@2.0.0"}), &caches).unwrap_err();
    assert!(error.contains("composer.lock has 3.5.0"), "{error}");
    // Locked dev dependency that is not installed: an honest error, nothing fetched.
    let error = run(&ws, &json!({"name": "phpunit/phpunit@10.5.2"}), &caches).unwrap_err();
    assert!(error.contains("not installed in vendor/"), "{error}");
}

const GEMFILE_LOCK: &str = "GEM
  remote: https://rubygems.org/
  specs:
    rack (3.0.8)
    nokogiri (1.16.0-arm64-darwin)
      racc (~> 1.4)
    racc (1.7.3)

GIT
  remote: https://github.com/example/tool.git
  revision: 0123456789abcdef
  specs:
    tool (0.4.0)

PATH
  remote: engines/local
  specs:
    local (0.1.0)

PLATFORMS
  arm64-darwin

DEPENDENCIES
  rack
";

#[test]
fn ruby_reads_locked_gems_from_the_bundle_path_and_gem_directories() {
    let project = tempfile::tempdir().unwrap();
    write(&project.path().join("Gemfile.lock"), GEMFILE_LOCK);
    write(&project.path().join("Gemfile"), "gem 'rack'\n");
    let bundle = project.path().join("vendor/bundle/ruby/3.3.0");
    write(
        &bundle.join("gems/rack-3.0.8/lib/rack.rb"),
        "module Rack\n  def self.needle; end\nend\n",
    );
    write(
        &bundle.join("bundler/gems/tool-0123456789ab/tool.gemspec"),
        "Gem::Specification.new\n",
    );
    // nokogiri is installed only in a gem directory outside the project.
    let gem_home = tempfile::tempdir().unwrap();
    write(
        &gem_home
            .path()
            .join("gems/nokogiri-1.16.0-arm64-darwin/lib/nokogiri.rb"),
        "module Nokogiri; end\n",
    );
    let caches = Caches {
        gem_paths: vec![gem_home.path().to_path_buf()],
        ..Default::default()
    };
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    let out = run(&ws, &json!({"name": "rack", "query": "needle"}), &caches).unwrap();
    assert!(
        out.starts_with("ruby rack 3.0.8 (resolved from Gemfile.lock GEM)"),
        "{out}"
    );
    assert!(out.contains("lib/rack.rb:2: def self.needle; end"), "{out}");
    let out = run(&ws, &json!({"name": "nokogiri@1.16.0"}), &caches).unwrap();
    assert!(
        out.starts_with("ruby nokogiri 1.16.0-arm64-darwin"),
        "{out}"
    );
    let out = run(&ws, &json!({"name": "tool"}), &caches).unwrap();
    assert!(
        out.starts_with("ruby tool 0.4.0 (resolved from Gemfile.lock GIT)"),
        "{out}"
    );
    // A dependency-of-a-dependency line is not a top-level spec of its own version.
    let error = run(&ws, &json!({"name": "racc"}), &caches).unwrap_err();
    assert!(
        error.contains("racc 1.7.3 is locked but not installed"),
        "{error}"
    );
    let error = run(&ws, &json!({"name": "local"}), &caches).unwrap_err();
    assert!(error.contains("path gem"), "{error}");
}

#[test]
fn nuget_reads_the_restored_version_from_the_global_packages_folder() {
    let project = tempfile::tempdir().unwrap();
    write(&project.path().join("App/App.csproj"), "<Project />\n");
    write(&project.path().join("App.sln"), "\n");
    write(
        &project.path().join("App/packages.lock.json"),
        r#"{"version":1,"dependencies":{"net8.0":{"Newtonsoft.Json":{"type":"Direct","resolved":"13.0.3"}}}}"#,
    );
    write(
        &project.path().join("App/obj/project.assets.json"),
        r#"{"libraries":{"Serilog/3.1.1":{"type":"package"},"App.Core/1.0.0":{"type":"project"}}}"#,
    );
    let packages = tempfile::tempdir().unwrap();
    write(
        &packages.path().join("newtonsoft.json/13.0.3/README.md"),
        "Json.NET needle\n",
    );
    write(
        &packages.path().join("serilog/3.1.1/serilog.nuspec"),
        "<package />\n",
    );
    let caches = Caches {
        nuget_packages: Some(packages.path().to_path_buf()),
        ..Default::default()
    };
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    let out = run(
        &ws,
        &json!({"name": "Newtonsoft.Json", "query": "needle"}),
        &caches,
    )
    .unwrap();
    assert!(
        out.starts_with("nuget Newtonsoft.Json 13.0.3 (resolved from App/packages.lock.json)"),
        "{out}"
    );
    assert!(out.contains("README.md:1: Json.NET needle"), "{out}");
    let out = run(&ws, &json!({"name": "serilog"}), &caches).unwrap();
    assert!(out.contains("App/obj/project.assets.json"), "{out}");
    assert!(out.contains("  serilog.nuspec\n"), "{out}");
    let error = run(&ws, &json!({"name": "App.Core"}), &caches).unwrap_err();
    assert!(error.contains("not in packages.lock.json"), "{error}");
}

fn sources_jar(path: &Path, files: &[(&str, &str)]) {
    use std::io::Write as _;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, body) in files {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

const POM: &str = r#"<?xml version="1.0"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <version>1.0.0</version>
  <properties><guava.version>33.0.0-jre</guava.version></properties>
  <dependencies>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>${guava.version}</version>
    </dependency>
    <dependency>
      <groupId>org.slf4j</groupId>
      <artifactId>slf4j-api</artifactId>
      <version>${undefined.version}</version>
    </dependency>
  </dependencies>
</project>
"#;

#[test]
fn jvm_reads_the_declared_version_inside_its_sources_jar() {
    let project = tempfile::tempdir().unwrap();
    write(&project.path().join("pom.xml"), POM);
    let m2 = tempfile::tempdir().unwrap();
    let guava = m2.path().join("com/google/guava/guava");
    sources_jar(
        &guava.join("33.0.0-jre/guava-33.0.0-jre-sources.jar"),
        &[
            ("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\n"),
            (
                "com/google/common/base/Strings.java",
                "package com.google.common.base;\npublic final class Strings { static void needle() {} }\n",
            ),
            ("com/google/common/collect/Lists.java", "class Lists {}\n"),
        ],
    );
    // An older cached version without sources; only binary jars for slf4j.
    write(&guava.join("31.1-jre/guava-31.1-jre.jar"), "binary");
    let slf4j = m2.path().join("org/slf4j/slf4j-api");
    write(&slf4j.join("2.0.9/slf4j-api-2.0.9.jar"), "binary");
    let caches = Caches {
        maven_repo: Some(m2.path().to_path_buf()),
        ..Default::default()
    };
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());

    let out = run(&ws, &json!({"name": "com.google.guava:guava"}), &caches).unwrap();
    assert!(
        out.starts_with(
            "jvm com.google.guava:guava 33.0.0-jre (resolved from pom.xml, local Maven repository)"
        ),
        "{out}"
    );
    assert!(out.contains("other locked versions: 31.1-jre"), "{out}");
    assert!(out.contains(".: 2 entries\n  META-INF/\n  com/\n"), "{out}");

    let out = run(
        &ws,
        &json!({"name": "com.google.guava:guava", "path": "com/google/common", "query": "needle"}),
        &caches,
    )
    .unwrap();
    assert!(
        out.contains("com/google/common/base/Strings.java:2: public final class Strings"),
        "{out}"
    );
    assert!(!out.contains("Lists"), "{out}");

    let out = run(
        &ws,
        &json!({"name": "com.google.guava:guava", "path": "com/google/common/base/Strings.java:2-2"}),
        &caches,
    )
    .unwrap();
    assert!(out.contains("\n2:public final class Strings"), "{out}");
    assert!(
        out.contains("[com/google/common/base/Strings.java lines 2-2 of 2]"),
        "{out}"
    );

    let error = run(
        &ws,
        &json!({"name": "com.google.guava:guava", "path": "../escape"}),
        &caches,
    )
    .unwrap_err();
    assert!(error.contains("without `..`"), "{error}");
    let error = run(
        &ws,
        &json!({"name": "com.google.guava:guava@31.1-jre"}),
        &caches,
    )
    .unwrap_err();
    assert!(error.contains("without a -sources.jar"), "{error}");
    // An unresolvable property version is skipped; the binary-only cache has no sources.
    let error = run(&ws, &json!({"name": "org.slf4j:slf4j-api"}), &caches).unwrap_err();
    assert!(error.contains("no -sources.jar"), "{error}");
    // A `:` name never falls through to another ecosystem.
    let error = run(
        &ws,
        &json!({"name": "com.google.guava:guava", "ecosystem": "npm"}),
        &caches,
    )
    .unwrap_err();
    assert!(error.contains("JVM group:artifact"), "{error}");
}

#[test]
fn jvm_uses_gradle_lockfiles_and_the_gradle_module_cache() {
    let project = tempfile::tempdir().unwrap();
    write(
        &project.path().join("gradle.lockfile"),
        "# comment\norg.jetbrains.kotlin:kotlin-stdlib:1.9.22=compileClasspath,runtimeClasspath\nempty=\n",
    );
    let gradle = tempfile::tempdir().unwrap();
    sources_jar(
        &gradle.path().join(
            "caches/modules-2/files-2.1/org.jetbrains.kotlin/kotlin-stdlib/1.9.22/0a1b2c/kotlin-stdlib-1.9.22-sources.jar",
        ),
        &[("kotlin/Standard.kt", "package kotlin\n")],
    );
    let caches = Caches {
        gradle_home: Some(gradle.path().to_path_buf()),
        ..Default::default()
    };
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    let out = run(
        &ws,
        &json!({"name": "org.jetbrains.kotlin:kotlin-stdlib"}),
        &caches,
    )
    .unwrap();
    assert!(
        out.starts_with("jvm org.jetbrains.kotlin:kotlin-stdlib 1.9.22 (resolved from gradle.lockfile, Gradle module cache)"),
        "{out}"
    );
    assert!(out.contains("  kotlin/\n"), "{out}");
}

#[test]
fn new_ecosystems_are_detected_and_named_in_the_schema() {
    let project = tempfile::tempdir().unwrap();
    for file in ["pom.xml", "Gemfile", "composer.json", "App.csproj"] {
        write(&project.path().join(file), "\n");
    }
    assert_eq!(
        Ecosystem::present(project.path()),
        [
            Ecosystem::Jvm,
            Ecosystem::Ruby,
            Ecosystem::Composer,
            Ecosystem::NuGet
        ]
    );
    let schema = DepSource.schema();
    assert_eq!(
        schema["function"]["parameters"]["properties"]["ecosystem"]["enum"],
        json!([
            "cargo", "npm", "python", "go", "jvm", "ruby", "composer", "nuget"
        ])
    );
    assert!(validate_name("com.google.guava:guava").is_ok());
    for bad in ["a:b:c", ":guava", "com..google:guava", "com.google:../x"] {
        assert!(validate_name(bad).is_err(), "{bad}");
    }
}
