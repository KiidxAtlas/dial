#!/usr/bin/env node
import { spawnSync } from 'node:child_process'
import { accessSync, constants, existsSync, realpathSync } from 'node:fs'
import { homedir } from 'node:os'
import { delimiter, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { runInstaller } from './install.mjs'

const SELF_PATH = safeRealpath(fileURLToPath(import.meta.url))
const INSTALL_FLAGS = new Set(['--install', '--reinstall', '--cli', '--cli-only', '--desktop', '--desktop-only'])

function safeRealpath(p) {
  try {
    return realpathSync(p)
  } catch {
    return resolve(p)
  }
}

function isExecutableFile(p, platform = process.platform) {
  if (!p || !existsSync(p)) return false
  try {
    accessSync(p, platform === 'win32' ? constants.F_OK : constants.X_OK)
    return true
  } catch {
    return false
  }
}

function isShimPath(p) {
  const normalized = p.replace(/\\/g, '/')
  return normalized === SELF_PATH.replace(/\\/g, '/')
    || normalized.includes('/node_modules/')
    || normalized.includes('/.bun/bin/')
    || normalized.includes('/.bun/install/')
    || normalized.endsWith('.mjs')
    || normalized.endsWith('.js')
}

export function findNativeDial(env = process.env, platform = process.platform, home = homedir()) {
  const exe = platform === 'win32' ? 'dial.exe' : 'dial'
  const candidates = []

  if (env.DIAL_INSTALL_DIR) {
    candidates.push(join(env.DIAL_INSTALL_DIR, exe))
  }
  candidates.push(join(home, '.local', 'bin', exe))
  if (platform === 'darwin') {
    candidates.push('/opt/homebrew/bin/dial', '/usr/local/bin/dial')
  }

  for (const entry of (env.PATH || '').split(delimiter)) {
    if (!entry) continue
    candidates.push(join(entry, exe))
  }

  const seen = new Set()
  for (const candidate of candidates) {
    if (!isExecutableFile(candidate, platform)) continue
    const real = safeRealpath(candidate)
    if (seen.has(real)) continue
    seen.add(real)
    if (isShimPath(real) || isShimPath(candidate)) continue
    return candidate
  }
  return null
}

export function isEphemeralRunner(argv = process.argv, env = process.env) {
  const scriptPath = (argv[1] || '').replace(/\\/g, '/')
  if (scriptPath.includes('/_npx/') || scriptPath.includes('/bunx-') || scriptPath.includes('/.bun/install/cache/')) {
    return true
  }
  return Boolean(env.npm_command === 'exec')
}

export function shouldRunInstaller(argv = process.argv, env = process.env, nativeBinary = null) {
  const args = argv.slice(2)
  if (args.some(arg => INSTALL_FLAGS.has(arg) || arg === '--desktop-version' || arg.startsWith('--desktop-version='))) {
    return true
  }
  if (!nativeBinary) return true
  // Bare `bunx @kiidxatlas/dial` or `npx @kiidxatlas/dial` with no CLI subcommand installs/updates Dial.
  if (args.length === 0 && isEphemeralRunner(argv, env)) return true
  return false
}

async function main() {
  const args = process.argv.slice(2)
  let nativeBinary = findNativeDial()

  if (shouldRunInstaller(process.argv, process.env, nativeBinary)) {
    const code = await runInstaller(args)
    if (code !== 0) process.exit(code)

    // If the user explicitly asked for install flags (or ran bare `bunx @kiidxatlas/dial`),
    // finish cleanly after installation.
    const hasInstallOnlyArgs = args.length === 0
      || args.every(a => INSTALL_FLAGS.has(a) || a === '--desktop-version' || a.startsWith('--desktop-version=') || a === '-h' || a === '--help')
    if (hasInstallOnlyArgs) process.exit(0)

    nativeBinary = findNativeDial()
    if (!nativeBinary) {
      process.stderr.write('error: dial was installed, but could not be located on disk\n')
      process.exit(1)
    }
  }

  const res = spawnSync(nativeBinary, args, { stdio: 'inherit' })
  if (res.error) {
    process.stderr.write(`error: failed to launch ${nativeBinary}: ${res.error.message}\n`)
    process.exit(1)
  }
  process.exit(res.status ?? 0)
}

const isMain = process.argv[1] && safeRealpath(process.argv[1]) === SELF_PATH
if (isMain) {
  main().catch((err) => {
    process.stderr.write(`error: ${err?.message ?? err}\n`)
    process.exit(1)
  })
}
