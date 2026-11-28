import assert from 'node:assert/strict'
import { createHash, generateKeyPairSync, sign } from 'node:crypto'
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'
import { parseInstallArgs, runInstaller } from '../bin/install.mjs'
import { findNativeDial, isEphemeralRunner, shouldRunInstaller } from '../bin/dial.mjs'
import { verifyDesktopAsset } from '../bin/verify-desktop.mjs'

test('Desktop asset verification fails closed on changed bytes, comments, or signing key', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'dial-desktop-signature-'))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  const path = join(dir, 'fixture')
  const payload = Buffer.from('desktop fixture')
  writeFileSync(path, payload)
  const { publicKey, privateKey } = generateKeyPairSync('ed25519')
  const keyId = Buffer.alloc(8, 1)
  const encode = (...lines) => Buffer.from(lines.join('\n')).toString('base64')
  const key = encode('untrusted comment: fixture', Buffer.concat([
    Buffer.from('Ed'), keyId, publicKey.export({ type: 'spki', format: 'der' }).subarray(-32)
  ]).toString('base64'))
  const sig = sign(null, createHash('blake2b512').update(payload).digest(), privateKey)
  const sigLine = Buffer.concat([Buffer.from('ED'), keyId, sig]).toString('base64')
  const global = sign(null, Buffer.concat([sig, Buffer.from('fixture')]), privateKey).toString('base64')
  const valid = encode('untrusted comment: fixture', sigLine, 'trusted comment: fixture', global)
  assert.equal(await verifyDesktopAsset(path, valid, key), true)
  assert.equal(await verifyDesktopAsset(path, valid), false)
  assert.equal(await verifyDesktopAsset(path, encode('', sigLine, 'trusted comment: changed', global), key), false)
  writeFileSync(path, 'changed bytes')
  assert.equal(await verifyDesktopAsset(path, valid, key), false)
})

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

test('runInstaller refuses both legacy remote installer overrides', async () => {
  for (const key of ['DIAL_INSTALL_SCRIPT_URL', 'DIAL_CLI_INSTALL_URL']) {
    await assert.rejects(
      runInstaller(['--cli'], { [key]: 'https://example.invalid/install.sh' }),
      /Remote installer script overrides are no longer supported/
    )
  }
})

test('runInstaller invokes only the sibling CLI script with the selected version', { skip: process.platform === 'win32' }, async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'dial-npm-run-'))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  for (const name of ['install.mjs', 'install-unix.sh']) {
    copyFileSync(new URL(`../bin/${name}`, import.meta.url), join(dir, name))
  }
  const outFile = join(dir, 'recorded.txt')
  writeFileSync(join(dir, 'install-cli.sh'), '#!/bin/sh\nprintf "%s|%s|%s\\n" "$*" "$DIAL_CLI_ONLY" "$DIAL_VERSION" > "$RECORD"\n')
  const { runInstaller: runBundled } = await import(pathToFileURL(join(dir, 'install.mjs')).href)
  assert.equal(await runBundled(['--cli', 'v0.4.1'], { PATH: process.env.PATH, RECORD: outFile }), 0)
  assert.equal(readFileSync(outFile, 'utf8').trim(), 'v0.4.1|1|v0.4.1')
})
