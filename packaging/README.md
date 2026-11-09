# Distribution packages

Package definitions for third-party registries. Each release updates the AUR files here together
with `Formula/dial.rb` (`scripts/update-formula.py --aur`), but pushing them to the AUR is manual
because it needs your AUR SSH key. The homebrew-core formula builds from the source tarball, whose
checksum is not in `SHA256SUMS`; bump it by hand when you submit an update.

## AUR: `aur/dial-bin`

Installs the signed release binary for x86_64 and aarch64 Linux and depends on `bubblewrap` for the
sandbox. To publish (needs an AUR account with an SSH key):

```sh
git clone ssh://aur@aur.archlinux.org/dial-bin.git aur-dial-bin
cp packaging/aur/dial-bin/PKGBUILD packaging/aur/dial-bin/.SRCINFO aur-dial-bin/
cd aur-dial-bin && makepkg -si   # test on an Arch machine first
git add PKGBUILD .SRCINFO && git commit -m "dial-bin 0.3.1" && git push
```

After editing `PKGBUILD`, regenerate `.SRCINFO` with `makepkg --printsrcinfo > .SRCINFO`.

## homebrew-core: `homebrew-core/dial.rb`

A build-from-source formula, as homebrew-core requires (it does not accept prebuilt binaries).
homebrew-core rejects projects that are not yet notable: roughly 75+ stars, or 30+ forks or
watchers, plus outside usage. Until then, users install from this repository's tap:

```sh
brew tap kiidxatlas/dial https://github.com/KiidxAtlas/dial
brew install dial
```

To submit, test locally first, then open a PR against `Homebrew/homebrew-core`:

```sh
brew install --build-from-source ./packaging/homebrew-core/dial.rb
brew test dial && brew audit --new --strict dial
```

## nixpkgs

`flake.nix` already builds Dial (`nix run github:KiidxAtlas/dial`). A nixpkgs submission needs a
`pkgs/by-name/di/dial/package.nix` with a `cargoHash` computed by `nix build` on a machine with Nix.
