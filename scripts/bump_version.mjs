// bump_version.mjs —— 发版自动更新版本号（release.sh 调用）
// 用法：node bump_version.mjs <desktopDir> [x.y.z]
// 同步更新 tauri.conf.json / Cargo.toml / package.json 三处版本；
// 未显式指定时 patch 段 +1。Cargo.lock 的本地包版本由 cargo build 自动同步。
import fs from 'node:fs'
import path from 'node:path'

const [dir, overrideRaw] = process.argv.slice(2)
if (!dir) {
  console.error('✗ 缺少参数：<desktopDir> [x.y.z]')
  process.exit(1)
}
const override = (overrideRaw || '').trim()
const confPath = path.join(dir, 'src-tauri/tauri.conf.json')
const conf = JSON.parse(fs.readFileSync(confPath, 'utf8'))
const oldV = conf.version
let newV
if (override) {
  if (!/^\d+\.\d+\.\d+$/.test(override)) {
    console.error('✗ 版本号格式非法: ' + override + '（应为 x.y.z）')
    process.exit(1)
  }
  newV = override
} else {
  const [a, b, c] = oldV.split('.').map(Number)
  newV = `${a}.${b}.${c + 1}`
}
if (newV === oldV) {
  console.log(oldV)
  process.exit(0)
}
// 1. tauri.conf.json（应用版本，右下角角标显示来源）
conf.version = newV
fs.writeFileSync(confPath, JSON.stringify(conf, null, 2) + '\n')
// 2. Cargo.toml（[package] 段第一处 version 行）
const cargoPath = path.join(dir, 'src-tauri/Cargo.toml')
const cargo = fs.readFileSync(cargoPath, 'utf8')
if (!/^version = "/m.test(cargo)) {
  console.error('✗ Cargo.toml 未找到 version 行')
  process.exit(1)
}
fs.writeFileSync(cargoPath, cargo.replace(/^version = ".*"$/m, `version = "${newV}"`))
// 3. package.json
const pkgPath = path.join(dir, 'package.json')
const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'))
pkg.version = newV
fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n')
console.log(newV)
