import assert from 'node:assert/strict'
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import http from 'node:http'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { parseInstallArgs, runInstaller } from '../bin/install.mjs'
import { findNativeDial, isEphemeralRunner, shouldRunInstaller } from '../bin/dial.mjs'

test('parseInstallArgs forwards CLI/Desktop flags cleanly', () => {
  assert.deepEqual(parseInstallArgs([]), {
    installerArgs: [],
    cliOnly: false,
    desktopOnly: false,
    desktopVersion: null,
    cliVersion: null,
    help: false
  })
  assert.deepEqual(parseInstallArgs(['--cli']), {
    installerArgs: ['--cli'],
    cliOnly: true,
    desktopOnly: false,
    desktopVersion: null,
    cliVersion: null,
    help: false
  })
  assert.deepEqual(parseInstallArgs(['--desktop', '--desktop-version', 'v0.4.0']), {
    installerArgs: ['--desktop', '--desktop-version', 'v0.4.0'],
    cliOnly: false,
    desktopOnly: true,
    desktopVersion: 'v0.4.0',
    cliVersion: null,
    help: false
  })
  assert.deepEqual(parseInstallArgs(['v0.4.1']), {
    installerArgs: ['v0.4.1'],
    cliOnly: false,
    desktopOnly: false,
    desktopVersion: null,
    cliVersion: 'v0.4.1',
    help: false
  })
  assert.deepEqual(parseInstallArgs(['--help']), {
    installerArgs: [],
    cliOnly: false,
    desktopOnly: false,
    desktopVersion: null,
    cliVersion: null,
    help: true
  })
})

test('findNativeDial locates an executable native binary and ignores shims', () => {
  const dir = mkdtempSync(join(tmpdir(), 'dial-npm-test-'))
  try {
    const binDir = join(dir, 'bin')
    const shimDir = join(dir, 'node_modules', '.bin')
    const nativePath = join(binDir, 'dial')
    const shimPath = join(shimDir, 'dial')

    mkdirSync(binDir, { recursive: true })
    mkdirSync(shimDir, { recursive: true })
    writeFileSync(shimPath, '#!/bin/sh\necho shim\n')
    chmodSync(shimPath, 0o755)
    writeFileSync(nativePath, '#!/bin/sh\necho native\n')
    chmodSync(nativePath, 0o755)

    assert.equal(
      findNativeDial({ PATH: shimDir }, 'darwin', join(dir, 'empty-home')),
      null,
      'must ignore node_modules shims'
    )
    assert.equal(
      findNativeDial({ PATH: `${shimDir}:${binDir}` }, 'darwin', join(dir, 'empty-home')),
      nativePath,
      'must resolve native dial binary'
    )
    assert.equal(
      findNativeDial({ DIAL_INSTALL_DIR: binDir, PATH: '' }, 'darwin', join(dir, 'empty-home')),
      nativePath,
      'must honor DIAL_INSTALL_DIR'
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('shouldRunInstaller distinguishes bunx installs from normal CLI invocations', () => {
  assert.equal(isEphemeralRunner(['bun', '/tmp/bunx-1234-@kiidxatlas/dial/bin/dial.mjs'], {}), true)
  assert.equal(isEphemeralRunner(['node', '/Users/u/.bun/bin/dial'], {}), false)

  assert.equal(shouldRunInstaller(['bun', '/tmp/bunx-1234/bin/dial.mjs'], {}, '/usr/local/bin/dial'), true)
  assert.equal(shouldRunInstaller(['node', '/Users/u/.bun/bin/dial', '--version'], {}, '/usr/local/bin/dial'), false)
  assert.equal(shouldRunInstaller(['node', '/Users/u/.bun/bin/dial', '--cli'], {}, '/usr/local/bin/dial'), true)
  assert.equal(shouldRunInstaller(['node', '/Users/u/.bun/bin/dial'], {}, null), true)
})

test('runInstaller downloads and executes the shell installer with forwarded args', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'dial-npm-run-'))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  const outFile = join(dir, 'recorded.txt')

  const server = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' })
    res.end(`#!/bin/sh\nprintf '%s|%s|%s|%s|%s\\n' "$*" "\${DIAL_CLI_ONLY:-0}" "\${DIAL_DESKTOP_ONLY:-0}" "\${DIAL_DESKTOP_VERSION:-}" "\${DIAL_VERSION:-}" > "${outFile}"\n`)
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  t.after(() => server.close())

  const { port } = server.address()
  const code = await runInstaller(
    ['--cli'],
    { ...process.env, DIAL_INSTALL_SCRIPT_URL: `http://127.0.0.1:${port}/install.sh` },
    'darwin'
  )
  assert.equal(code, 0)
  assert.equal(readFileSync(outFile, 'utf8').trim(), '--cli|1|0||')

  const desktopCode = await runInstaller(
    ['--desktop', '--desktop-version', 'v0.4.0', 'v0.4.1'],
    { ...process.env, DIAL_INSTALL_SCRIPT_URL: `http://127.0.0.1:${port}/install.sh` },
    'darwin'
  )
  assert.equal(desktopCode, 0)
  assert.equal(readFileSync(outFile, 'utf8').trim(), '--desktop --desktop-version v0.4.0 v0.4.1|0|1|v0.4.0|v0.4.1')
})
