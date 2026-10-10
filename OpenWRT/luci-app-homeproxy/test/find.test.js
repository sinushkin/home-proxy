// Проверка выбора службы страницей LuCI (hp-router или vps-client) на заглушках: node test/find.test.js
const fsmod = require('fs');
const src = fsmod.readFileSync('../htdocs/luci-static/resources/view/homeproxy/control.js', 'utf8');
function load(execImpl) {
  const view = { extend: (o) => o };
  const fs = { exec: execImpl };
  const ui = {};
  const E = (tag, attrs, kids) => ({ tag, attrs, kids });
  return new Function('view', 'fs', 'ui', 'E', src.replace(/^'use strict';/, '').replace(/^'require .*$/mg, ''))(view, fs, ui, E);
}
async function check(name, execImpl, expect) {
  const v = load(execImpl);
  const r = await v.load();
  const got = r instanceof Error ? 'ERR:' + r.message : `${r.service.name}:${r.text}`;
  console.log((got === expect ? 'ok   ' : 'FAIL ') + name + ' -> ' + got);
}
(async () => {
  const notFound = () => Promise.reject(new Error('Entry not found'));
  await check('only vps-client installed', (bin) => bin.includes('vps-client') ? Promise.resolve({ code: 0, stdout: 'homeproxy-control://192.168.3.1:47001/KEY\n' }) : notFound(), 'vps-client:homeproxy-control://192.168.3.1:47001/KEY');
  await check('hp-router wins when both work', (bin) => Promise.resolve({ code: 0, stdout: bin + '\n' }), 'hp-router:/usr/bin/hp-router');
  await check('neither installed', notFound, 'ERR:На роутере нет службы Home Proxy (ни hp-router, ни vps-client).');
  await check('installed but control off', (bin) => bin.includes('vps-client') ? Promise.resolve({ code: 1, stdout: '', stderr: 'управление выключено' }) : notFound(), 'ERR:управление выключено');
  await check('hp-router off, vps-client works', (bin) => bin.includes('hp-router') ? Promise.resolve({ code: 1, stderr: 'управление выключено' }) : Promise.resolve({ code: 0, stdout: 'S\n' }), 'vps-client:S');
})();
