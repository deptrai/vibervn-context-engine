#!/usr/bin/env node

const { spawn, execFileSync } = require('child_process')
const { existsSync } = require('fs')
const path = require('path')

const PLATFORMS = {
  'linux-x64': 'vibervn-context-engine-linux-x64',
  'linux-arm64': 'vibervn-context-engine-linux-arm64',
  'darwin-arm64': 'vibervn-context-engine-darwin-arm64',
  'win32-x64': 'vibervn-context-engine-win32-x64',
}

const platformKey = `${process.platform}-${process.arch}`
const packageName = PLATFORMS[platformKey]

if (!packageName) {
  console.error(
    `Unsupported platform: ${process.platform} ${process.arch}\n` +
    `Supported platforms: ${Object.keys(PLATFORMS).join(', ')}`
  )
  process.exit(1)
}

let binPath
try {
  const binName = process.platform === 'win32' ? 'context-engine-rs.exe' : 'context-engine-rs'
  binPath = path.join(
    path.dirname(require.resolve(`${packageName}/package.json`)),
    'bin',
    binName
  )
} catch {
  console.error(
    `Could not find the binary package "${packageName}".\n` +
    `This usually means it was not installed (e.g., --no-optional was used) or your platform is unsupported.\n` +
    `Try reinstalling: npm install -g vibervn-context-engine`
  )
  process.exit(1)
}

if (!existsSync(binPath)) {
  console.error(
    `Binary not found at "${binPath}".\n` +
    `The platform package "${packageName}" is installed but the binary is missing.\n` +
    `Try reinstalling: npm install -g vibervn-context-engine`
  )
  process.exit(1)
}

// Run the binary as a CHILD we own, not a synchronous block, so we can forward
// stop signals and — critically on Windows — tear down the WHOLE process tree.
//
// Why this matters: the binary runs in "router" mode and spawns one child
// "worker" process per repo. Those workers hold a RocksDB lock AND keep the
// `context-engine-rs.exe` image open. A worker only self-exits after an idle
// window (default 300s), so if the router dies but its workers linger, the next
// `npx vibervn-context-engine@latest` cannot replace the cached binary and npm
// fails with `EPERM: operation not permitted, unlink ...context-engine-rs.exe`.
//
// On Windows there is no real SIGINT to forward: Node's `child.kill('SIGINT')`
// maps to TerminateProcess, which kills ONLY the router and leaves the workers
// orphaned. So on Windows we kill the whole tree with `taskkill /T /F` (the
// router is still alive when we snapshot, so its workers are in the tree). On
// POSIX we forward the real signal to the child, which the router handles to
// kill its workers before exiting. The binary's Job Object (Windows
// kill-on-close) remains the backstop for anything either path races past.
const child = spawn(binPath, process.argv.slice(2), {
  stdio: 'inherit',
  env: process.env,
})

// Stop signals we forward / translate. Listed once so we can both register and
// (in the exit handler) DE-register them — see the re-raise note below.
const STOP_SIGNALS = ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGBREAK']

let terminating = false
let exited = false

function terminate(signal) {
  if (terminating) return
  terminating = true

  // Guard: if the child is already gone, do NOTHING. Two reasons:
  //  - there is nothing left to kill, and
  //  - on Windows `taskkill /T /F <pid>` against a dead PID could match a REUSED
  //    pid and kill an unrelated process tree. `exited` is set by the 'exit'
  //    handler; `child.exitCode !== null` also means it has exited; `child.pid`
  //    is undefined if spawn never succeeded (the 'error' handler ran).
  if (exited || child.exitCode !== null || child.pid === undefined) return

  if (process.platform === 'win32') {
    // Kill the router AND its worker children as one tree. /F so a worker that
    // ignores the polite close still dies and releases the .exe + RocksDB lock.
    // The child is still alive here (guarded above), so its workers are in the
    // tree snapshot taskkill takes. Wrapped: a child that dies in the gap makes
    // taskkill exit non-zero (execFileSync throws) — caught, then a best-effort
    // direct kill (also caught) so the handler can never crash the wrapper.
    try {
      execFileSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], {
        stdio: 'ignore',
      })
    } catch {
      try {
        child.kill()
      } catch {}
    }
  } else {
    // POSIX: forward the real signal; the router kills its workers, then exits.
    try {
      child.kill(signal || 'SIGTERM')
    } catch {}
  }
}

// Forward the usual stop signals. SIGINT = Ctrl+C; SIGTERM = supervisor/kill;
// SIGHUP = terminal closed (POSIX). Windows only delivers SIGINT/SIGBREAK here.
// Each is registered exactly once; the `terminating` guard makes the body
// idempotent if several arrive (or one arrives twice).
for (const sig of STOP_SIGNALS) {
  process.on(sig, () => terminate(sig))
}

child.on('error', (err) => {
  console.error(`Failed to launch context-engine binary: ${err.message}`)
  process.exit(1)
})

child.on('exit', (code, signal) => {
  exited = true
  if (signal) {
    // Re-raise the signal on ourselves so our exit status reflects it, matching
    // normal child-process signal semantics. We MUST drop our own signal
    // listeners first: otherwise re-raising e.g. SIGINT would just re-enter
    // `terminate()` (a no-op now, but it would also stop the default
    // termination), leaving the wrapper hung instead of exiting. (Mostly a POSIX
    // path — on Windows child death surfaces as an exit code, not a signal.)
    for (const sig of STOP_SIGNALS) process.removeAllListeners(sig)
    process.kill(process.pid, signal)
    return
  }
  process.exit(code ?? 0)
})
