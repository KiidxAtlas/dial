#!/usr/bin/env node
import { spawnSync } from 'node:child_process'
import { existsSync, realpathSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

export function parseInstallArgs(argv) {
  const installerArgs = []
  let cliOnly = false
  let desktopOnly = false
  let desktopVersion = null
  let cliVersion = null
  let help = false

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]
    if (arg === '--cli' || arg === '--cli-only') {
      cliOnly = true
      desktopOnly = false
      installerArgs.push('--cli')
    } else if (arg === '--desktop' || arg === '--desktop-only') {
      desktopOnly = true
      cliOnly = false
      installerArgs.push('--desktop')
    } else if (arg === '--desktop-version') {
      const next = argv[++i]
      if (!next) throw new Error('--desktop-version requires a value')
      desktopVersion = next
      installerArgs.push('--desktop-version', next)
    } else if (arg.startsWith('--desktop-version=')) {
      const value = arg.slice('--desktop-version='.length)
      if (!value) throw new Error('--desktop-version requires a value')
      desktopVersion = value
      installerArgs.push(arg)
    } else if (arg === '-h' || arg === '--help') {
      help = true
    } else if (arg === '--install' || arg === '--reinstall') {
      // Explicit install trigger handled by wrapper.
    } else {
      if (!arg.startsWith('-') && !cliVersion) cliVersion = arg
      installerArgs.push(arg)
    }
  }

  return { installerArgs, cliOnly, desktopOnly, desktopVersion, cliVersion, help }
}

function printHelp() {
  process.stdout.write([
    'Dial installer via Bun / npm (@kiidxatlas/dial)',
    '',
    'Usage:',
    '  bunx @kiidxatlas/dial                  Install Dial Desktop + dial CLI',
    '  bunx @kiidxatlas/dial --cli            Install only the dial CLI',
    '  bunx @kiidxatlas/dial --desktop        Install only Dial Desktop',
    '  bun install -g @kiidxatlas/dial        Install global launcher shim',
    '',
    'Environment:',
    '  DIAL_CLI_ONLY=1                        Install only the dial CLI',
    '  DIAL_INSTALL_DIR=<dir>                 Override CLI install directory',
    '  DIAL_APP_DIR=<dir>                     Override Desktop install directory',
    ''
  ].join('\n'))
}

export async function runInstaller(argv = process.argv.slice(2), env = process.env, platform = process.platform) {
  const { installerArgs, cliOnly, desktopOnly, desktopVersion, cliVersion, help } = parseInstallArgs(argv)
  if (help) {
    printHelp()
    return 0
  }

  if (env.DIAL_INSTALL_SCRIPT_URL || env.DIAL_CLI_INSTALL_URL) {
    throw new Error('Remote installer script overrides are no longer supported; this package runs bundled installers.')
  }

  const childEnv = { ...env, DIAL_NPM_RUNTIME: process.execPath }
  if (cliOnly) childEnv.DIAL_CLI_ONLY = '1'
  if (desktopOnly) childEnv.DIAL_DESKTOP_ONLY = '1'
  if (desktopVersion) childEnv.DIAL_DESKTOP_VERSION = desktopVersion
  if (cliVersion) childEnv.DIAL_VERSION = cliVersion

  const isWindows = platform === 'win32'
  const scriptPath = fileURLToPath(new URL(isWindows ? 'install-windows.ps1' : 'install-unix.sh', import.meta.url))
  const res = isWindows
    ? spawnSync('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', scriptPath], {
      stdio: 'inherit', env: childEnv
    })
    : spawnSync('/bin/sh', [scriptPath, ...installerArgs], { stdio: 'inherit', env: childEnv })
  if (res.error) throw res.error
  return res.status ?? 1
}

const entry = process.argv[1]
const isMain = entry && existsSync(entry) && realpathSync(entry) === realpathSync(fileURLToPath(import.meta.url))
if (isMain) {
  runInstaller().then(
    code => process.exit(code),
    (err) => {
      process.stderr.write(`error: ${err?.message ?? err}\n`)
      process.exit(1)
    }
  )
}
