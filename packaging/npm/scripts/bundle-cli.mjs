import { createPublicKey } from 'node:crypto'
import { readFileSync, writeFileSync } from 'node:fs'

// Like the release workflow, pair the source installers with the trusted release key at packaging time.
const configuredKey = process.env.DIAL_RELEASE_PUBLIC_KEY_PEM
if (!configuredKey) throw new Error('DIAL_RELEASE_PUBLIC_KEY_PEM is required to pack the signed CLI installers')
const key = createPublicKey({ key: configuredKey, format: 'pem' })
if (key.asymmetricKeyType !== 'ed25519') throw new Error('The Engine release public key must be Ed25519')
const pem = key.export({ type: 'spki', format: 'pem' }).trimEnd()
const source = (name) => readFileSync(new URL(`../../../scripts/${name}`, import.meta.url), 'utf8')
const save = (name, content) => writeFileSync(new URL(`../bin/${name}`, import.meta.url), content)

const sh = source('install.sh')
const shebang = '#!/bin/sh\n'
if (!sh.startsWith(shebang)) throw new Error('Unexpected install.sh shebang')
const ps1 = source('install.ps1')
const placeholder = /^    \$DialReleasePublicKeyPem = ''\r?$/gm
if ([...ps1.matchAll(placeholder)].length !== 1) throw new Error('Expected exactly one install.ps1 key placeholder')

save('install-cli.sh', `${shebang}export DIAL_RELEASE_PUBLIC_KEY='${pem}'\n${sh.slice(shebang.length)}`)
save('install-cli.ps1', ps1.replace(placeholder, () => `    $DialReleasePublicKeyPem = @'\n${pem}\n'@`))
