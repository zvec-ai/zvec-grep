#!/usr/bin/env python3
"""Generate the deterministic terminal tour used by the project READMEs.

The install scene mirrors the interactive UI and output implemented in
src/cli/commands.ts. Run with:

  uv run --with pillow scripts/readme-demo.py

Generate a visual comparison variant with:

  uv run --with pillow scripts/readme-demo.py --variant accent
"""

import argparse
from itertools import cycle
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


WIDTH = 1200
HEIGHT = 720
BACKGROUND = "#0d1117"
PANEL = "#161b22"
BORDER = "#30363d"
TEXT = "#e6edf3"
MUTED = "#8b949e"
GREEN = "#3fb950"
BLUE = "#58a6ff"
PURPLE = "#bc8cff"
YELLOW = "#d29922"
CYAN = "#39c5cf"

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / ".github" / "assets" / "zvec-grep-tour.gif"
VARIANT_OUTPUTS = {
    "baseline": ROOT / ".github" / "assets" / "zvec-grep-tour-00-baseline.gif",
    "accent": ROOT / ".github" / "assets" / "zvec-grep-tour-01-accent.gif",
    "hierarchy": ROOT / ".github" / "assets" / "zvec-grep-tour-02-hierarchy.gif",
    "block": OUTPUT,
}
FRAME_MS = 70
LINE_HEIGHT = 29
MAX_LINES = 19
SHELL_TITLE = "zvec-grep — ~/code"
INPUT_PROMPTS = {"$ ", "› "}
INPUT_TEXT = "#f0f6fc"
INPUT_BLOCK = "#111d2b"
ACTIVE_VARIANT = "block"


def load_font(size: int, bold: bool = False):
    candidates = []
    if bold:
        candidates.extend(
            [
                "/System/Library/Fonts/SFNSMono.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf",
            ]
        )
    candidates.extend(
        [
            "/System/Library/Fonts/SFNSMono.ttf",
            "/System/Library/Fonts/Menlo.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        ]
    )
    for candidate in candidates:
        if Path(candidate).exists():
            return ImageFont.truetype(candidate, size=size)
    return ImageFont.load_default()


FONT = load_font(20)
FONT_BOLD = load_font(20, bold=True)
FONT_TITLE = load_font(20, bold=True)


def part(value, color=TEXT, bold=False):
    return (value, color, FONT_BOLD if bold else FONT)


def line(*parts):
    return list(parts)


def plain(value="", color=TEXT, bold=False):
    return line(part(value, color, bold))


def terminal(title=SHELL_TITLE):
    image = Image.new("RGB", (WIDTH, HEIGHT), BACKGROUND)
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle(
        (24, 24, WIDTH - 24, HEIGHT - 24),
        radius=14,
        fill=PANEL,
        outline=BORDER,
        width=2,
    )
    draw.rectangle((25, 68, WIDTH - 25, HEIGHT - 25), fill=BACKGROUND)
    for x, color in ((52, "#ff5f56"), (80, "#ffbd2e"), (108, "#27c93f")):
        draw.ellipse((x - 7, 39, x + 7, 53), fill=color)
    draw.text(
        (WIDTH // 2, 46),
        title,
        font=FONT_TITLE,
        fill=MUTED,
        anchor="mm",
    )
    return image


def visible(lines):
    return lines[-MAX_LINES:]


def is_input_line(current):
    return bool(current) and current[0][0] in INPUT_PROMPTS


def render(lines, title=SHELL_TITLE):
    image = terminal(title)
    draw = ImageDraw.Draw(image)
    y = 92
    for current in visible(lines):
        input_line = is_input_line(current)
        if ACTIVE_VARIANT == "block" and input_line:
            draw.rounded_rectangle(
                (43, y - 4, WIDTH - 43, y + LINE_HEIGHT - 3),
                radius=6,
                fill=INPUT_BLOCK,
            )
            draw.rounded_rectangle(
                (43, y - 4, 47, y + LINE_HEIGHT - 3),
                radius=2,
                fill=GREEN,
            )

        if ACTIVE_VARIANT == "hierarchy" and not input_line:
            draw.rectangle((62, y - 2, 64, y + LINE_HEIGHT - 2), fill=BORDER)
            x = 79
        else:
            x = 55

        for index, (value, color, font) in enumerate(current):
            if input_line and ACTIVE_VARIANT != "baseline":
                if index == 0:
                    color = GREEN
                    font = FONT_BOLD
                elif value != "█":
                    color = INPUT_TEXT
                    font = FONT_BOLD
            draw.text((x, y), value, font=font, fill=color)
            x += draw.textlength(value, font=font)
        y += LINE_HEIGHT
    return image


def hold(frames, lines, repeats, title=None):
    image = render(lines, title or SHELL_TITLE)
    frames.extend(image.copy() for _ in range(repeats))


def type_command(frames, lines, command, stride=1, title=None):
    timing = cycle((1, 1, 2, 1, 1, 1, 2))
    for count in range(stride, len(command) + stride, stride):
        typed = command[: min(count, len(command))]
        current = lines + [line(part("$ ", GREEN, True), part(typed), part("█", MUTED))]
        hold(frames, current, next(timing), title)
    lines.append(line(part("$ ", GREEN, True), part(command)))
    hold(frames, lines, 5, title)


def reveal(frames, lines, additions, pauses=None, title=None):
    pauses = pauses or [3] * len(additions)
    for addition, pause in zip(additions, pauses):
        lines.append(addition)
        hold(frames, lines, pause, title)


def chooser_lines(active):
    agents = [
        ("Claude Code", "detected"),
        ("Codex", "detected"),
        ("OpenCode", "not found"),
        ("Cursor", "not found"),
    ]
    result = []
    for index, (label, status) in enumerate(agents):
        marker = "●" if index == active else "○"
        marker_color = GREEN if index == active else MUTED
        result.append(
            line(
                part("  "),
                part(marker, marker_color, index == active),
                part(f" {label:<11}  "),
                part(status, MUTED),
            )
        )
    result.extend(
        [
            plain(),
            plain("  Use ↑↓ to move · Enter to select", MUTED),
        ]
    )
    return result


def progress_line(indexed, total, spinner):
    width = 18
    percent = round(indexed * 100 / total)
    filled = round(width * percent / 100)
    bar = "█" * filled + "░" * (width - filled)
    return line(
        part("│", MUTED),
        part(f"  {spinner} ", GREEN),
        part("Indexing files  "),
        part(bar, GREEN),
        part(f"  {percent:>3}%  {indexed}/{total}  4 workers", MUTED),
    )


def install_scene(frames):
    lines = []
    type_command(frames, lines, "npm install -g @zvec/zvec-grep", stride=2)
    lines.append(plain())
    type_command(frames, lines, "zg install")

    reveal(
        frames,
        lines,
        [
            plain(),
            plain("zvec-grep setup", CYAN, True),
            plain("─" * 40, MUTED),
            plain(),
            plain("Choose agent integrations", TEXT, True),
            plain(),
        ],
        [2, 3, 2, 1, 4, 1],
    )
    lines.extend(chooser_lines(0))
    hold(frames, lines, 18)

    lines[-6:] = chooser_lines(1)
    hold(frames, lines, 12)

    reveal(
        frames,
        lines,
        [
            plain(),
            plain("Installing integrations", TEXT, True),
            plain(),
            line(part("  ✓", GREEN, True), part(" Codex")),
            plain("    MCP       configured", MUTED),
            plain(),
            line(part("  ✓", GREEN, True), part(" Server")),
            plain("    ready at http://127.0.0.1:7999/mcp", MUTED),
            plain(),
            plain("zvec-grep is ready", CYAN, True),
            plain(),
            plain("  Agents       Codex"),
            plain("  Remote data  Authorization requested on first remote use"),
            plain(),
            plain(
                "Restart the selected agents or start a new session to load the integration.",
                MUTED,
            ),
        ],
        [2, 4, 1, 4, 3, 1, 4, 4, 1, 5, 1, 3, 5, 1, 12],
    )
    return lines


def index_scene(frames, lines):
    repository_title = "zvec-grep — ~/code/your-repository"
    lines.append(plain())
    hold(frames, lines, 6, SHELL_TITLE)
    type_command(frames, lines, "cd your-repository", stride=2, title=SHELL_TITLE)
    type_command(
        frames,
        lines,
        "zg index --embedding local/potion-code-16m-v2",
        stride=2,
        title=repository_title,
    )
    lines.append(plain("│  · Scanning workspace", MUTED))
    hold(frames, lines, 8, repository_title)

    spinners = cycle(("·", "✢", "✳", "✶", "✻", "✽"))
    for indexed in (8, 23, 47, 76, 111, 148, 176, 192):
        lines[-1] = progress_line(indexed, 192, next(spinners))
        hold(frames, lines, 4 if indexed < 192 else 12, repository_title)
    return lines


def agent_scene(frames, shell_lines):
    repository_title = "zvec-grep — ~/code/your-repository"
    shell_lines.append(plain())
    hold(frames, shell_lines, 5, repository_title)
    type_command(
        frames,
        shell_lines,
        "codex",
        title=repository_title,
    )

    title = "Codex — ~/code/your-repository"
    reveal(
        frames,
        shell_lines,
        [
            plain(),
            plain("╭────────────────────────────────────────────────────────╮", MUTED),
            line(
                part("│ >_ ", MUTED),
                part("OpenAI Codex", TEXT, True),
                part(" (v0.145.0-alpha.27)                    │", MUTED),
            ),
            plain("│                                                        │", MUTED),
            line(
                part("│ model:     ", MUTED),
                part("gpt-5.6-sol xhigh fast", TEXT),
                part("   /model to change   │", MUTED),
            ),
            line(
                part("│ directory: ", MUTED),
                part("~/code/your-repository", TEXT),
                part("                 │", MUTED),
            ),
            plain("╰────────────────────────────────────────────────────────╯", MUTED),
        ],
        [2, 2, 3, 1, 2, 2, 5],
        title,
    )

    for spinner in ("◐", "◓", "◑", "◒"):
        booting = shell_lines + [
            line(
                part(f"{spinner} ", CYAN),
                part("Booting MCP server: zvec_grep", TEXT),
                part("  (0s • esc to interrupt)", MUTED),
            )
        ]
        hold(frames, booting, 2, title)

    prompt = "My app forgets dark mode every time I refresh. Find out why."
    timing = cycle((1, 1, 2, 1, 2, 1))
    for count in range(2, len(prompt) + 2, 2):
        typed = prompt[: min(count, len(prompt))]
        hold(
            frames,
            shell_lines
            + [
                line(part("› ", TEXT, True), part(typed), part("█", MUTED)),
                plain(),
                line(
                    part("  gpt-5.6-sol xhigh fast", TEXT),
                    part(" · ", MUTED),
                    part("~/code/your-repository", TEXT),
                ),
            ],
            next(timing),
            title,
        )
    shell_lines.append(line(part("› ", TEXT, True), part(prompt)))
    shell_lines.append(plain())
    hold(frames, shell_lines, 5, title)

    for elapsed, spinner in enumerate(("◐", "◓", "◑", "◒", "◐")):
        working = shell_lines + [
            line(
                part(f"{spinner} ", CYAN),
                part("Working", TEXT),
                part(f" ({elapsed}s • esc to interrupt)", MUTED),
            )
        ]
        hold(frames, working, 2, title)

    reveal(
        frames,
        shell_lines,
        [
            line(
                part("• ", TEXT, True),
                part("I’ll trace how the theme preference is saved and restored."),
            ),
            plain(),
            line(
                part("• ", TEXT, True),
                part("zvec_grep_search", YELLOW),
            ),
            line(part("  └ ", MUTED), part("freshness: fresh", TEXT)),
            line(
                part("    ", MUTED),
                part("src/theme/use-theme.ts:12-36", BLUE),
            ),
            plain("    matched: 16-18", MUTED),
            plain("    source:", MUTED),
            plain('    15  export function useTheme() {'),
            plain('    16    const [theme, setTheme] = useState("light");'),
            plain("    17    useEffect(() => saveTheme(theme), [theme]);"),
            plain(),
            line(
                part("    ", MUTED),
                part("src/storage/preferences.ts:1-16", BLUE),
            ),
            plain("    matched: 6-8", MUTED),
            plain("    source:", MUTED),
            plain("    6  export function loadTheme() {"),
            plain('    7    return localStorage.getItem("theme");'),
            plain(),
            line(
                part("• ", TEXT, True),
                part("Found it — the app saves your choice, but never reads"),
            ),
            plain("  it back when it starts."),
        ],
        [5, 1, 5, 2, 2, 2, 2, 3, 3, 3, 1, 2, 2, 2, 3, 3, 1, 4, 60],
        title,
    )


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--variant",
        choices=tuple(VARIANT_OUTPUTS),
        default="block",
        help="visual treatment to render",
    )
    return parser.parse_args()


def main():
    global ACTIVE_VARIANT
    args = parse_args()
    ACTIVE_VARIANT = args.variant
    output = VARIANT_OUTPUTS[args.variant]

    frames = []
    shell_lines = install_scene(frames)
    shell_lines = index_scene(frames, shell_lines)
    agent_scene(frames, shell_lines)

    output.parent.mkdir(parents=True, exist_ok=True)
    frames[0].save(
        output,
        save_all=True,
        append_images=frames[1:],
        duration=FRAME_MS,
        loop=0,
        optimize=True,
        disposal=2,
    )
    print(
        f"wrote {output.relative_to(ROOT)} "
        f"({len(frames)} frames, {output.stat().st_size:,} bytes)"
    )


if __name__ == "__main__":
    main()
