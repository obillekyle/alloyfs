#!/usr/bin/env bun
/**
 * The terminal screenshot in the README, generated from a real agent run.
 *
 *     bun run scripts/terminal-shot.ts    # writes assets/shot.html and the SVGs
 *
 * A Bun script in a Rust repository, because the same generator produces the
 * screenshots for the other two projects and one recipe beats three. It touches
 * nothing the crates depend on: it runs the release binary and writes into
 * `assets/`.
 *
 * **Why a screenshot at all.** The README claims an agent that exports a folder,
 * watches it with the OS-native watcher, and accepts sessions from clients.
 * That claim is worth more as the agent saying it than as a paragraph, and the
 * log is coloured on purpose — green for the level, dim for the timestamp and
 * the module, italic for the structured fields — which a monochrome fence
 * throws away.
 *
 * **Generated, not captured by hand.** An export is built in a temp directory,
 * the agent is started on a spare port, a real `ping` is made against it so the
 * session rows are a client that actually connected, and the agent is torn
 * down. Refreshing the image after an output change is one command.
 *
 * **Two panes, because GitHub serves README images through its own proxy**,
 * which strips the CSS a `prefers-color-scheme` rule inside the image would
 * need. `<picture>` does the choosing instead, so there is a light file and a
 * dark one.
 */
import { existsSync } from 'node:fs'

/** GitHub's own light and dark palettes — the image sits inside a GitHub page. */
const THEMES = {
  dark: {
    bg: '#0d1117',
    chrome: '#161b22',
    border: '#30363d',
    fg: '#c9d1d9',
    grey: '#8b949e',
    red: '#ff7b72',
    green: '#7ee787',
    yellow: '#d29922',
    blue: '#4493f8',
    cyan: '#79c0ff',
    magenta: '#d2a8ff',
    title: '#8b949e',
  },
  light: {
    bg: '#ffffff',
    chrome: '#f6f8fa',
    border: '#d0d7de',
    fg: '#24292f',
    grey: '#6e7781',
    red: '#cf222e',
    green: '#1a7f37',
    yellow: '#9a6700',
    blue: '#0969da',
    cyan: '#0550ae',
    magenta: '#8250df',
    title: '#6e7781',
  },
} as const

/** Widened to `string`: the two palettes share keys, not values. */
type Theme = Record<keyof (typeof THEMES)['dark'], string>

interface Run {
  text: string
  colour: keyof Theme
  bold: boolean
  dim: boolean
  italic: boolean
}

/**
 * ANSI SGR into runs.
 *
 * `tracing_subscriber`'s default formatter is the whole palette here: dim for
 * the timestamp, the module path and the `=` between a field and its value,
 * italic for the field names, and one colour per level. Anything unrecognised
 * resets rather than guessing, since a wrong colour is a claim about the output
 * that is not true.
 */
function parse(line: string): Run[] {
  const runs: Run[] = []
  let colour: keyof Theme = 'fg'
  let bold = false
  let dim = false
  let italic = false
  let at = 0

  const SGR = /\x1b\[([0-9;]*)m/g
  let m: RegExpExecArray | null
  const push = (text: string) => {
    if (text) runs.push({ text, colour, bold, dim, italic })
  }

  while ((m = SGR.exec(line))) {
    push(line.slice(at, m.index))
    at = m.index + m[0].length
    for (const code of (m[1] || '0').split(';')) {
      if (code === '1') bold = true
      else if (code === '2') dim = true
      else if (code === '3') italic = true
      else if (code === '31') colour = 'red'
      else if (code === '32') colour = 'green'
      else if (code === '33') colour = 'yellow'
      else if (code === '34') colour = 'blue'
      else if (code === '35') colour = 'magenta'
      else if (code === '36') colour = 'cyan'
      else if (code === '37') colour = 'fg'
      else if (code === '90') colour = 'grey'
      else {
        colour = 'fg'
        bold = false
        dim = false
        italic = false
      }
    }
  }
  push(line.slice(at))
  return runs
}

const esc = (s: string) =>
  s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')

const strip = (line: string) => line.replace(/\x1b\[[0-9;]*m/g, '')

const TITLE = 'projects — alloyfs'

function pane(lines: string[], theme: Theme, name: string): string {
  const body = lines
    .map(line => {
      const spans = parse(line)
        .map(r => {
          const style = [
            `color:${theme[r.colour]}`,
            r.bold ? 'font-weight:600' : '',
            r.dim ? 'opacity:.65' : '',
            r.italic ? 'font-style:italic' : '',
          ]
            .filter(Boolean)
            .join(';')
          return `<span style="${style}">${esc(r.text)}</span>`
        })
        .join('')
      return spans || '&nbsp;'
    })
    .join('\n')

  return `<div class="win" id="${name}" style="--bg:${theme.bg};--chrome:${theme.chrome};--border:${theme.border};--title:${theme.title}">
  <div class="bar"><i class="d r"></i><i class="d y"></i><i class="d g"></i><span class="t">${TITLE}</span></div>
  <pre>${body}</pre>
</div>`
}

const root = new URL('..', import.meta.url).pathname.replace(
  /^\/([A-Za-z]:)/,
  '$1',
)

const exe = process.platform === 'win32' ? 'alloyfs.exe' : 'alloyfs'
const BIN = [`${root}/target/release/${exe}`, `${root}/target/debug/${exe}`].find(
  existsSync,
)
if (!BIN) {
  console.error(`no ${exe} in target/. Build one first: cargo build --release`)
  process.exit(1)
}

// A small export, so the agent has something real to open and watch. Built here
// rather than checked in, so the image is of the agent actually running.
const dir = `${(process.env.TEMP ?? '/tmp').replaceAll('\\', '/')}/alloyfs-shot`
await Bun.$`rm -rf ${dir}`.nothrow().quiet()
for (const [path, body] of [
  ['notes/todo.md', '- ship the mount\n'],
  ['src/main.rs', 'fn main() {}\n'],
]) {
  await Bun.write(`${dir}/${path}`, body)
}

/** A spare port: a screenshot must not depend on 7440 being free, or take it. */
const PORT = 7457

const agent = Bun.spawn(
  [BIN, 'serve', '--tcp', `127.0.0.1:${PORT}`, '--export', `projects=${dir}`],
  {
    // The agent logs to stderr on purpose — stdout is the `--stdio` transport.
    env: { ...process.env, CLICOLOR_FORCE: '1', RUST_LOG: 'info' },
    stdout: 'ignore',
    stderr: 'pipe',
  },
)

const dec = new TextDecoder()
const reader = agent.stderr.getReader()
let raw = ''

/** Read until `probe` shows up in what has arrived so far, or time runs out. */
async function readUntil(probe: RegExp, ms: number): Promise<boolean> {
  const deadline = Date.now() + ms
  while (!probe.test(strip(raw)) && Date.now() < deadline) {
    const next = await Promise.race([
      reader.read(),
      new Promise<null>(r => setTimeout(() => r(null), 1000)),
    ])
    if (next && !next.done) raw += dec.decode(next.value)
    else if (next?.done) break
  }
  return probe.test(strip(raw))
}

if (!(await readUntil(/listening \(tcp\)/, 15_000))) {
  agent.kill()
  console.error('the agent never reported a listening socket')
  process.exit(1)
}

/**
 * A genuine client, so the session rows are not staged.
 *
 * `events` rather than `ping`, and started without waiting on it, because the
 * shape of the log depends on it. A client that connects and immediately exits
 * puts its own teardown in the buffer before the next read, and the agent
 * reports that teardown as a warning about a connection closed by the peer —
 * true, but an artifact of the screenshot rather than anything a reader needs.
 * An `events` tail attaches and stays attached, so the capture ends on the
 * client arriving, which is the thing being shown.
 */
const client = Bun.spawn(
  [BIN, 'events', `tcp://127.0.0.1:${PORT}/projects`],
  { stdout: 'ignore', stderr: 'ignore' },
)
await readUntil(/client attached/, 10_000)

// Both, and by tree: an agent left holding the port makes the next run fail to
// bind, and a stray `events` tail keeps a session open against whatever agent
// is listening afterwards.
client.kill()
agent.kill()
if (process.platform === 'win32') {
  for (const pid of [client.pid, agent.pid]) {
    Bun.spawnSync(['taskkill', '/PID', String(pid), '/T', '/F'], {
      stdout: 'ignore',
      stderr: 'ignore',
    })
  }
}

const lines = raw
  .split('\n')
  .map(l => l.replace(/\r$/, ''))
  // Three cosmetic edits, and they are the only ones made to the output:
  //   * the temp directory the export happened to live in becomes the path the
  //     README uses, including dropping the `\\?\` prefix Windows canonicalises
  //     it to;
  //   * the port goes back to 7440, which is the default the README documents;
  //   * the client identifies itself as the user and host that ran this, which
  //     is not something to publish. Matched on the quoted value rather than on
  //     `client="..."`, because the formatter italicises field names and leaves
  //     escape sequences sitting between the name and its `=`, so the obvious
  //     pattern matches nothing and the name ships.
  // Every other character, the timings included, is the agent's own.
  .map(l =>
    l
      .replaceAll(`\\\\?\\${dir.replaceAll('/', '\\')}`, '/home/you/projects')
      .replaceAll(dir, '/home/you/projects')
      .replaceAll(`:${PORT}`, ':7440')
      .replace(/"[^"]*@[^"]*"/g, '"you@desktop"'),
  )

while (lines.length && !strip(lines[lines.length - 1] as string).trim())
  lines.pop()

if (!lines.length) {
  console.error('no output captured from the agent')
  process.exit(1)
}

const html = `<!doctype html>
<meta charset="utf-8">
<title>alloyfs — terminal</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;600&display=swap" rel="stylesheet">
<style>
  body { margin: 0; padding: 40px; background: #8b8b8b; display: flex; flex-direction: column; gap: 40px; align-items: flex-start; }
  .win {
    background: var(--bg); border: 1px solid var(--border); border-radius: 10px;
    overflow: hidden; width: max-content; box-shadow: 0 8px 30px rgba(0,0,0,.18);
  }
  .bar {
    height: 34px; background: var(--chrome); border-bottom: 1px solid var(--border);
    display: flex; align-items: center; gap: 8px; padding: 0 13px;
  }
  .d { width: 11px; height: 11px; border-radius: 50%; display: inline-block; }
  .r { background: #ff5f57 } .y { background: #febc2e } .g { background: #28c840 }
  .t {
    margin-left: 10px; font-size: 12px; color: var(--title);
    /* Google Sans if this machine has it, and the usual UI stack if not. */
    font-family: "Google Sans", "Google Sans Text", ui-sans-serif, system-ui, "Segoe UI", Roboto, sans-serif;
  }
  pre {
    margin: 0; padding: 18px 22px 20px;
    font-family: "JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: 13px; line-height: 1.62; letter-spacing: 0;
    white-space: pre; tab-size: 2;
    /* Ligatures off, and this is not taste. JetBrains Mono draws two dashes as
       one long dash, so a flag in this window would render differently from how
       it has to be typed. */
    font-variant-ligatures: none;
    font-feature-settings: "liga" 0, "calt" 0;
  }
</style>
${pane(lines, THEMES.dark, 'dark')}
${pane(lines, THEMES.light, 'light')}
`

const asset = (name: string) => `${root}/assets/${name}`

await Bun.write(asset('shot.html'), html)
console.log(`  assets/shot.html  (${lines.length} lines, for previewing)`)

/**
 * The same output as SVG, which is what the README embeds.
 *
 * **Flowing tspans, not pinned ones.** Computing an `x` per span from a fixed
 * advance would draw correctly only in the font it was measured against, and a
 * reader without JetBrains Mono would get every span overlapping the last.
 * Letting them flow means any monospace lays the line out correctly and only
 * the total width changes, so the font stack degrades instead of breaking. It
 * is also why this embeds no font: a few KB of text rather than 160 KB of
 * base64, and nothing to re-subset when a glyph appears.
 */
function svg(theme: Theme): string {
  const cols = Math.max(...lines.map(l => strip(l).length))
  const FONT = 13
  const LINE = FONT * 1.62
  const PAD = 20
  const CHROME = 34
  const w = Math.ceil(cols * FONT * 0.6 + PAD * 2)
  const h = Math.ceil(lines.length * LINE + PAD * 2 + CHROME)

  const rows = lines
    .map((line, i) => {
      const y = (PAD + CHROME + (i + 0.85) * LINE).toFixed(1)
      const spans = parse(line)
        .map(r => {
          const style = [
            `fill:${theme[r.colour]}`,
            r.bold ? 'font-weight:600' : '',
            r.dim ? 'opacity:.65' : '',
            r.italic ? 'font-style:italic' : '',
          ]
            .filter(Boolean)
            .join(';')
          return `<tspan style="${style}" xml:space="preserve">${esc(r.text)}</tspan>`
        })
        .join('')
      return spans
        ? `<text x="${PAD}" y="${y}" xml:space="preserve">${spans}</text>`
        : ''
    })
    .filter(Boolean)
    .join('\n    ')

  return `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" role="img" aria-label="the alloyfs agent starting: an export opened and watched, a TCP socket listening, then a client session attaching to it">
  <rect width="${w}" height="${h}" rx="10" fill="${theme.bg}" stroke="${theme.border}"/>
  <path d="M0 10a10 10 0 0 1 10-10h${w - 20}a10 10 0 0 1 10 10v${CHROME - 10}H0z" fill="${theme.chrome}"/>
  <line x1="0" y1="${CHROME}" x2="${w}" y2="${CHROME}" stroke="${theme.border}"/>
  <circle cx="19" cy="17" r="5.5" fill="#ff5f57"/>
  <circle cx="38" cy="17" r="5.5" fill="#febc2e"/>
  <circle cx="57" cy="17" r="5.5" fill="#28c840"/>
  <text x="76" y="21" font-family="Google Sans, Google Sans Text, ui-sans-serif, system-ui, Segoe UI, Roboto, sans-serif" font-size="11.5" fill="${theme.title}">${TITLE}</text>
  <g font-family="JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace" font-size="${FONT}" style="font-variant-ligatures:none;font-feature-settings:'liga' 0,'calt' 0">
    ${rows}
  </g>
</svg>
`
}

for (const [name, theme] of Object.entries(THEMES)) {
  await Bun.write(asset(`terminal-${name}.svg`), svg(theme))
  console.log(`  assets/terminal-${name}.svg`)
}
