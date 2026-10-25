use super::*;
use std::process::Command as Cmd;

fn skill(dir: &Path, name: &str, body: &str) {
    let root = dir.join(name);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(SKILL_FILE),
        format!("---\nname: {name}\ndescription: Test skill {name}\n---\n{body}\n"),
    )
    .unwrap();
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dial-skills-test-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn copies_nested_skills_and_skips_git_metadata() {
    let root = scratch("nested");
    let repo = root.join("repo");
    skill(&repo.join("skills"), "pdf", "body");
    skill(&repo.join("skills"), "docx", "body");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::write(repo.join(".git/config"), "x").unwrap();
    let dest = root.join("dest");
    fs::create_dir_all(&dest).unwrap();
    let names = copy_skills(&repo, None, &dest).unwrap();
    assert_eq!(names, vec!["docx".to_owned(), "pdf".to_owned()]);
    assert!(dest.join("pdf").join(SKILL_FILE).is_file());
    assert!(!dest.join(".git").exists());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn subpath_restricts_the_copy_and_refuses_escapes() {
    let root = scratch("subpath");
    let repo = root.join("repo");
    skill(&repo.join("skills"), "pdf", "body");
    skill(&repo, "other", "body");
    let dest = root.join("dest");
    fs::create_dir_all(&dest).unwrap();
    assert_eq!(
        copy_skills(&repo, Some("skills"), &dest).unwrap(),
        vec!["pdf".to_owned()]
    );
    assert!(!dest.join("other").exists());
    let escape = copy_skills(&repo, Some("../outside"), &root.join("dest2"));
    assert!(escape.is_err());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn refuses_symlinks_inside_a_skill() {
    let root = scratch("symlink");
    let repo = root.join("repo");
    skill(&repo, "pdf", "body");
    let outside = root.join("secret.txt");
    fs::write(&outside, "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, repo.join("pdf/leak.txt")).unwrap();
    let dest = root.join("dest");
    fs::create_dir_all(&dest).unwrap();
    #[cfg(unix)]
    {
        let error = copy_skills(&repo, None, &dest).unwrap_err().to_string();
        assert!(error.contains("symbolic link refused"), "{error}");
    }
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn ignores_symlinked_skill_directories_outside_the_package() {
    let root = scratch("linkdir");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let elsewhere = root.join("elsewhere");
    skill(&root, "elsewhere", "body");
    std::os::unix::fs::symlink(&elsewhere, repo.join("linked")).unwrap();
    let dest = root.join("dest");
    fs::create_dir_all(&dest).unwrap();
    let error = copy_skills(&repo, None, &dest).unwrap_err().to_string();
    assert!(error.contains("no skills found"), "{error}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rejects_a_skill_without_description() {
    let root = scratch("nodesc");
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("bad")).unwrap();
    fs::write(
        repo.join("bad").join(SKILL_FILE),
        "---\nname: bad\n---\nbody\n",
    )
    .unwrap();
    let dest = root.join("dest");
    fs::create_dir_all(&dest).unwrap();
    assert!(
        copy_skills(&repo, None, &dest)
            .unwrap_err()
            .to_string()
            .contains("description")
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn manifest_detects_content_and_mode_changes() {
    let root = scratch("manifest");
    skill(&root, "pdf", "one");
    let first = manifest(&root).unwrap();
    fs::write(root.join("pdf").join(SKILL_FILE), "changed").unwrap();
    let second = manifest(&root).unwrap();
    assert_ne!(integrity(&first), integrity(&second));
    assert_eq!(integrity(&second), integrity(&manifest(&root).unwrap()));
    assert!(second.keys().any(|k| k == "pdf/SKILL.md"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn fetches_from_a_local_bare_repository_and_detects_new_commits() {
    if Cmd::new("git").arg("--version").output().is_err() {
        return;
    }
    let root = scratch("fetch");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let git = |args: &[&str], dir: &Path| {
        let ok = Cmd::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "--quiet"], &work);
    skill(&work, "pdf", "v1");
    git(&["add", "."], &work);
    git(&["commit", "--quiet", "-m", "one"], &work);
    let bare = root.join("remote.git");
    git(
        &[
            "clone",
            "--quiet",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
        &root,
    );

    let source = super::super::source::parse(bare.to_str().unwrap()).unwrap();
    let cancel = AtomicBool::new(false);
    let first = fetch(&source, &root, &cancel, &mut |_| {}).unwrap();
    assert_eq!(first.commit.len(), 40);

    skill(&work, "pdf", "v2");
    git(&["commit", "--quiet", "-am", "two"], &work);
    git(&["push", "--quiet", bare.to_str().unwrap(), "main"], &work);
    let second = fetch(&source, &root, &cancel, &mut |_| {}).unwrap();
    assert_ne!(first.commit, second.commit);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn local_resolution_preserves_remote_identities() {
    let root = scratch("resolve");
    fs::create_dir_all(root.join("repo")).unwrap();
    assert_eq!(
        resolve_local("./repo", &root),
        fs::canonicalize(root.join("repo"))
            .unwrap()
            .to_str()
            .unwrap()
    );
    for url in [
        "https://host/repo",
        "file:///tmp/repo",
        "git@host:repo",
        "host:repo",
    ] {
        assert_eq!(resolve_local(url, &root), url);
    }
    // Even an existing path that resembles an scp URL must not change its remote identity.
    #[cfg(unix)]
    {
        fs::create_dir_all(root.join("host:repo")).unwrap();
        assert_eq!(resolve_local("host:repo", &root), "host:repo");
    }
    let _ = fs::remove_dir_all(root);
}
