import { createHash, verify } from 'node:crypto'
import { createReadStream, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

// Tauri updater key from dial-desktop/src-tauri/tauri.release.conf.json.
const desktopKey = 'dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDlBRERBOEM2MkQ5MThDQUYKUldTdmpKRXR4cWpkbW9PUnBZQ3lrM2xjajJrQTZjWlJEcGJpM0pPTklyRmhlR1BXSUpiQlZGemMK'

export async function verifyDesktopAsset(assetPath, encodedSignature, encodedKey = desktopKey) {
  const [, keyLine] = Buffer.from(encodedKey, 'base64').toString('utf8').split(/\r?\n/)
  const [, signatureLine, comment, globalLine] = Buffer.from(encodedSignature, 'base64').toString('utf8').split(/\r?\n/)
  if (!keyLine || !signatureLine || !comment?.startsWith('trusted comment: ') || !globalLine) return false
  const rawKey = Buffer.from(keyLine, 'base64')
  const rawSignature = Buffer.from(signatureLine, 'base64')
  const global = Buffer.from(globalLine, 'base64')
  // Current Tauri releases use prehashed minisign signatures (ED).
  if (rawKey.length !== 42 || rawKey.toString('ascii', 0, 2) !== 'Ed' ||
      rawSignature.length !== 74 || rawSignature.toString('ascii', 0, 2) !== 'ED' ||
      global.length !== 64 || !rawKey.subarray(2, 10).equals(rawSignature.subarray(2, 10))) return false
  const key = { key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), rawKey.subarray(10)]), format: 'der', type: 'spki' }
  const signature = rawSignature.subarray(10)
  if (!verify(null, Buffer.concat([signature, Buffer.from(comment.slice('trusted comment: '.length))]), key, global)) return false
  const hash = createHash('blake2b512')
  for await (const chunk of createReadStream(assetPath)) hash.update(chunk)
  return verify(null, hash.digest(), key, signature)
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    if (!await verifyDesktopAsset(process.argv[2], readFileSync(process.argv[3], 'utf8'))) {
      throw new Error('signature does not match the pinned Dial Desktop key')
    }
  } catch (error) {
    console.error(`Dial Desktop signature verification failed: ${error.message}`)
    process.exitCode = 1
  }
}
