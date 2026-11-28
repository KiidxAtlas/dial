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
For installed copies, `pacman -Qo /usr/bin/dial` identifies the owning package. Update
`dial-bin` through your AUR helper or rebuild the updated PKGBUILD with `makepkg -si`;
`dial update` leaves the package-owned binary alone.

## Homebrew tap (`Formula/dial.rb` and `Casks/dial-desktop.rb`)

This repository serves as a Homebrew tap for both the CLI (`Formula/dial.rb`) and macOS Dial Desktop (`Casks/dial-desktop.rb`, which depends on `kiidxatlas/dial/dial`):

```sh
# macOS: Dial Desktop + dial CLI
brew tap kiidxatlas/dial https://github.com/KiidxAtlas/dial
brew install --cask dial-desktop

# CLI only (macOS or Linux)
brew tap kiidxatlas/dial https://github.com/KiidxAtlas/dial
brew install dial
```
Homebrew reads versions from the published tap, not the latest GitHub tag. For a Desktop release, update the cask version and both macOS archive SHA-256 values from that **Desktop** release and publish the change to the tap. CLI release packaging PRs update the formula, not the cask. Users run `brew update`, then `brew upgrade --cask dial-desktop` for the app and `brew upgrade dial` for the CLI.


## Bun / npm wrapper (`packaging/npm`, `@kiidxatlas/dial`)

A zero-dependency launcher/installer wrapper for `bunx @kiidxatlas/dial` and `bun install -g @kiidxatlas/dial`. It runs package-bundled installers without requiring Bun `postinstall` trust, and delegates subsequent `dial` calls to the native binary.

Set the Engine release verification public key before publishing; see the [npm publishing instructions](npm/README.md#publishing).

```sh
cd packaging/npm
npm test
npm publish --access public
```

## homebrew-core: `homebrew-core/dial.rb`

A build-from-source formula, as homebrew-core requires (it does not accept prebuilt binaries).
homebrew-core rejects projects that are not yet notable: roughly 75+ stars, or 30+ forks or
watchers, plus outside usage. Until then, users install from this repository's tap above.

To submit, test locally first, then open a PR against `Homebrew/homebrew-core`:

```sh
brew install --build-from-source ./packaging/homebrew-core/dial.rb
brew test dial && brew audit --new --strict dial
```

## nixpkgs

`flake.nix` already builds Dial (`nix run github:KiidxAtlas/dial`). A nixpkgs submission needs a
`pkgs/by-name/di/dial/package.nix` with a `cargoHash` computed by `nix build` on a machine with Nix.
