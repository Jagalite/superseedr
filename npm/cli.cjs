#!/usr/bin/env node
'use strict';

const { spawn } = require('node:child_process');
const { constants } = require('node:os');

const supported = ['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-x64'];
const platform = `${process.platform}-${process.arch}`;
let binary;
try {
  if (!supported.includes(platform)) throw new Error(`Unsupported platform: ${platform}.`);
  if (process.platform === 'linux' && !process.report.getReport().header.glibcVersionRuntime) {
    throw new Error('The npm Linux binaries require glibc; musl/Alpine is not supported.');
  }
  binary = require.resolve(`superseedr-${platform}/bin/superseedr${process.platform === 'win32' ? '.exe' : ''}`);
} catch (error) {
  console.error(`superseedr: ${error.message}\nInstall with optional dependencies enabled, or use a native installer: https://github.com/Jagalite/superseedr/releases`);
  process.exit(1);
}

// Inherit the terminal so the native TUI retains raw input and rendering.
const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit' });
const handlers = new Map();
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  const handler = () => { if (child.exitCode === null && child.signalCode === null) child.kill(signal); };
  handlers.set(signal, handler);
  process.on(signal, handler);
}
child.on('error', (error) => {
  console.error(`superseedr: could not start native binary: ${error.message}`);
  process.exitCode = 1;
});
child.on('exit', (code, signal) => {
  for (const [name, handler] of handlers) process.removeListener(name, handler);
  if (signal && process.platform !== 'win32') process.kill(process.pid, signal);
  else process.exitCode = code ?? (128 + (constants.signals[signal] || 1));
});
