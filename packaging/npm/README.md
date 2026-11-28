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

On first run (or whenever invoked with `bunx @kiidxatlas/dial` / `dial-install`), the wrapper fetches and runs the official verified installer from `https://dial.rootatlas.dev/install.sh` (macOS/Linux) or `https://dial.rootatlas.dev/install.ps1` (Windows). On macOS this installs `Dial.app` into `/Applications` (or `~/Applications`) and `dial` into `~/.local/bin` without browser-quarantined `.dmg` prompts.

## Publishing

```sh
cd packaging/npm
npm test
npm publish --access public
```
