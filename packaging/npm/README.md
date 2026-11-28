# `@kiidxatlas/dial`

Lightweight Bun / npm installer and launcher wrapper for **Dial Desktop** and the **dial CLI**.

## Install

Install both **Dial Desktop** and the **`dial` CLI** in one step:

```sh
bunx @kiidxatlas/dial
```

Install only the CLI:

```sh
bunx @kiidxatlas/dial --cli
```

Or install globally as a launcher shim (works without Bun `postinstall` trust):

```sh
bun install -g @kiidxatlas/dial
dial
```

On first run (or whenever invoked with `bunx @kiidxatlas/dial` / `dial-install`), the wrapper runs the installer shipped in the package. The bundled scripts fetch release assets, not executable installer scripts. `DIAL_INSTALL_SCRIPT_URL` and `DIAL_CLI_INSTALL_URL` overrides are no longer accepted. On macOS this installs `Dial.app` into `/Applications` (or `~/Applications`) and `dial` into `~/.local/bin` without browser-quarantined `.dmg` prompts.

## Publishing

```sh
cd packaging/npm
npm test
npm publish --access public
```

Before packing or publishing, set `DIAL_RELEASE_PUBLIC_KEY_PEM` to the same trusted Ed25519 **public** key used by the Engine release workflow. `prepack` embeds that key into the canonical CLI installers from `../../scripts/`; packing fails without it. No signing key is needed.

The two platform launchers in `bin/` are snapshots of the website installers, with remote CLI script loading replaced by calls to the package-local scripts. Preserve that local delegation when refreshing the snapshots.
