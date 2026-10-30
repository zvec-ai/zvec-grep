// Render captured production text as annotated review sheets, not fabricated terminal output.
// node rust/scripts/render-cli-style.mjs fixtures.json output-dir [capture.json ...]
// Screenshot the generated HTML at viewport width 1120; catalog.md indexes every card.
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

const [input, destination, ...captures] = process.argv.slice(2);
if (!input || !destination) throw new Error('Expected captures.json and output directory');
const fixtures = [];
for (const path of [input, ...captures]) {
  fixtures.push(...JSON.parse(await readFile(path, 'utf8')));
}
if (new Set(fixtures.map(card => card.id)).size !== fixtures.length) {
  throw new Error('Duplicate card IDs: captures must have distinct identities');
}
await mkdir(destination, { recursive: true });
const escape = text => String(text).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');
const provenance = card => card.provenance || (card.title.includes('PTY') ? 'real PTY' : 'renderer fixture');
const palette = ['#111916', '#ed8e87', '#6fcf91', '#d9bb75', '#82a9dd', '#c49cce', '#8bbfbd', '#dce4df'];
function ansiHtml(text) {
  let color = '', background = '', dim = false, bold = false, result = '', start = 0;
  const append = part => {
    if (part) result += `<span style="${color ? `color:${color};` : ''}${background ? `background:${background};` : ''}${dim ? 'opacity:.55;' : ''}${bold ? 'font-weight:700;' : ''}">${escape(part)}</span>`;
  };
  // Captures select a frame before conversion. NUL is visibly annotated, never silently lost.
  const content = text.replace(/\x1b\[(?![\d;]*m)[\d;?]*[A-Za-z]/g, '').replaceAll('\r', '').replaceAll('\0', '␀').trimEnd();
  for (const match of content.matchAll(/\x1b\[([\d;]*)m/g)) {
    append(content.slice(start, match.index));
    const codes = (match[1] || '0').split(';').map(Number);
    for (let i = 0; i < codes.length; i++) {
      const code = codes[i];
      if (code === 0) { color = ''; background = ''; dim = false; bold = false; }
      else if (code === 1) bold = true;
      else if (code === 2) dim = true;
      else if (code === 22) { dim = false; bold = false; }
      else if (code === 39) color = '';
      else if (code === 49) background = '';
      else if ((code === 38 || code === 48) && codes[i + 1] === 2) {
        const rgb = `rgb(${codes.slice(i + 2, i + 5).join(',')})`;
        if (code === 38) color = rgb; else background = rgb;
        i += 4;
      } else if (code >= 30 && code <= 37) color = palette[code - 30];
      else if (code >= 90 && code <= 97) color = palette[code - 90];
      else if (code >= 40 && code <= 47) background = palette[code - 40];
    }
    start = match.index + match[0].length;
  }
  append(content.slice(start));
  return result;
}
const styles = `
*{box-sizing:border-box}body{margin:0;padding:36px;background:#0b100e;color:#dce4df;font-family:system-ui,sans-serif;width:1120px}
header{margin:0 0 24px}h1{font-size:25px;letter-spacing:-.4px;font-weight:600;margin:0 0 8px}header p{font-size:13px;color:#8e9d94;margin:0}
section{background:#111916;border:1px solid #28342d;border-radius:12px;margin:14px 0 0;padding:19px 24px}
.label{font-size:11px;letter-spacing:.6px;color:#a5b3aa;margin-bottom:8px;text-transform:uppercase}
.evidence{font-size:12px;color:#8e9d94;margin-bottom:12px}.command{font:14px/1.5 Menlo,Consolas,monospace;color:#85978b;margin-bottom:14px}
pre{font:17px/1.55 Menlo,Consolas,monospace;font-variant-ligatures:none;white-space:pre-wrap;overflow-wrap:anywhere;margin:0}
.note,footer{font-size:12px;color:#8e9d94;margin-top:18px}a{color:#6fcf91}
`;
const manifest = [];
async function sheet(name, title, cards, overview = false) {
  const body = cards.map(card => `<section id="${escape(card.id)}"><div class="label">${escape(card.title)}</div><div class="evidence">${escape(provenance(card))}${card.exitCode === undefined ? '' : ` · exit ${card.exitCode}`} · ${escape(card.id)}</div><div class="command">$ ${escape(card.command)}</div><pre>${ansiHtml(card.ansi)}</pre>${card.ansi.length ? '' : '<div class="note">Empty output (0 bytes). This annotation is outside the terminal.</div>'}${card.ansi.includes('\0') ? '<div class="note">␀ visualizes the actual NUL separator. Raw capture retains the NUL byte.</div>' : ''}${card.normalization ? `<div class="note">${escape(card.normalization)}</div>` : ''}</section>`).join('');
  await writeFile(join(destination, `${name}.html`), `<!doctype html><html lang="en"><meta charset="utf-8"><title>${escape(title)}</title><style>${styles}</style><header><h1>zg · ${escape(title)}</h1><p>Captured production renderers · green progress · neutral text · dim secondary information</p></header>${body}<footer>Cards and evidence labels are review annotations. Renderer fixtures use sample data; only real PTY cards execute the stated CLI command. A still image does not verify animation timing.</footer></html>`);
  manifest.push({ name, title, image: `${name}.png`, overview, cards: cards.map(card => ({ id: card.id, command: card.command, provenance: provenance(card), ...(card.exitCode === undefined ? {} : { exitCode: card.exitCode }) })) });
}
const overviews = [
  ['index-lifecycle', 'Index lifecycle', ['indexing', 'optimizing', 'saving', 'finishing', 'ready', 'error']],
  ['command-output', 'Consistent command output', ['status', 'config', 'auth', 'refresh', 'partial', 'drop']],
  ['interaction', 'Keyboard interactions', ['model-picker', 'install-picker', 'consent']],
];
for (const [name, title, ids] of overviews) {
  await sheet(name, title, ids.map(id => fixtures.find(card => card.id === id)).filter(Boolean), true);
}
const categories = [
  ['help', 'Help and version', id => /^cli-(help|no-arguments|version)/.test(id)],
  ['cli-failures', 'CLI failures and exit codes', id => /^cli-(bad-|drop-needs|auth-invalid|config-invalid)/.test(id)],
  ['integrations', 'Integration commands', id => /^cli-(install|uninstall)/.test(id) || id.startsWith('upgrade-')],
  ['server', 'Server lifecycle', id => id.startsWith('cli-server-')],
  ['query', 'Indexed query and previews', id => /^(query-|preview-)/.test(id)],
  ['paths', 'Path lookup, ripgrep and file streams', id => /^(lookup-|rg-|files-)/.test(id)],
  ['errors', 'Error states and diagnostic modes', id => /^error(-|$)/.test(id) || id === 'cancelled'],
  ['status', 'Workspace index states', id => /^status(-|$)/.test(id)],
  ['index', 'Index lifecycle and outcomes', id => /^(index|cli-index-|optimizing$|saving$|finishing$|ready$|partial$|drop$|refresh(?:-|$))/.test(id)],
  ['configuration', 'Configuration and authorization', id => /^(config(?:-|$)|auth(?:-|$)|cli-auth)/.test(id)],
  ['prompts', 'Interactive prompts', () => true],
];
const assigned = new Set();
for (const [group, title, accepts] of categories) {
  const cards = fixtures.filter(card => !assigned.has(card.id) && accepts(card.id));
  let page = [], lines = 0, index = 1;
  const flush = async () => {
    if (!page.length) return;
    await sheet(`catalog-${group}-${String(index++).padStart(2, '0')}`, title, page);
    page = []; lines = 0;
  };
  for (const card of cards) {
    const count = card.ansi.split('\n').length + 7;
    if (page.length && (page.length >= 4 || lines + count > 70)) await flush();
    page.push(card); lines += count; assigned.add(card.id);
  }
  await flush();
}
await writeFile(join(destination, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
const cell = value => String(value).replaceAll('|', '\\|').replaceAll('\n', ' ');
const rows = manifest.filter(page => !page.overview).flatMap(page => page.cards.map(card => `| [${card.id}](${page.image}) | \`${cell(card.command)}\` | ${card.provenance} | ${card.exitCode ?? '—'} |`));
let coverage;
try { coverage = JSON.parse(await readFile(join(destination, 'coverage.json'), 'utf8')); }
catch (error) { if (error.code !== 'ENOENT') throw error; }
const coverageNote = coverage
  ? '\n- Protocol-only entry points and shared target variants are explained in the [command coverage table](coverage.md) and [machine-readable inventory](coverage.json).'
  : '';
await writeFile(join(destination, 'catalog.md'), `# CLI review gallery\n\nEvery captured card is listed below. Review annotations are outside the terminal.\n\n- **real PTY**: the CLI command was executed; its exit code is recorded.\n- **renderer fixture**: the production formatter rendered sample data; this does not claim a successful business operation.\n- **PTY renderer fixture**: a production interactive prompt was invoked with sample data and cancelled, without saving credentials or granting access.${coverageNote}\n\n| Screenshot / card | Command or prompt entry point | Evidence | Exit |\n| --- | --- | --- | --- |\n${rows.join('\n')}\n`);
if (coverage) {
  const cardImages = new Map(manifest.filter(page => !page.overview).flatMap(page => page.cards.map(card => [card.id, page.image])));
  const coverageRows = coverage.coverage.map(row => {
    const links = row.cards.map(id => {
      if (!cardImages.has(id)) throw new Error(`Coverage references missing card: ${id}`);
      return `[${id}](${cardImages.get(id)})`;
    });
    return `| ${row.family} | \`${cell(row.command)}\` | ${cell(row.evidence)} | ${links.join(', ') || '—'} | ${cell(row.notes)} |`;
  });
  await writeFile(join(destination, 'coverage.md'), `# CLI command coverage\n\nAll public command families and subcommands are enumerated here. This is a presentation review, not an execution matrix for every flag combination, operating system, model or provider. Only **real PTY** entries claim command execution.\n\n${coverage.scope.targetCoverage}\n\nMCP stdout remains a protocol stream; no human-facing screenshot is invented for it. Still images cannot verify timing, readiness or byte-stream contracts; those require tests.\n\n| Family | Command / interaction | Evidence | Screenshots | Scope and limits |\n| --- | --- | --- | --- | --- |\n${coverageRows.join('\n')}\n`);
}
console.log(JSON.stringify({ cards: fixtures.length, sheets: manifest.length, output: destination }));
