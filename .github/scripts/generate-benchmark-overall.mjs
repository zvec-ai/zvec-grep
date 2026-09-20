import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const outputDir = join(dirname(fileURLToPath(import.meta.url)), "..", "assets");

const themes = {
  dark: {
    background: "#0d1117",
    panel: "#161b22",
    border: "#30363d",
    text: "#f0f6fc",
    muted: "#8b949e",
    grid: "#30363d",
    baseline: "#8b949e",
    green: "#3fb950",
    blue: "#58a6ff",
    purple: "#bc8cff",
    orange: "#ff7b32",
    qualityBadgeText: "#0d1117",
    resourceBadgeText: null,
  },
  light: {
    background: "#ffffff",
    panel: "#f6f8fa",
    border: "#d0d7de",
    text: "#1f2328",
    muted: "#656d76",
    grid: "#d8dee4",
    baseline: "#afb8c1",
    green: "#2da44e",
    blue: "#218bff",
    purple: "#a371f7",
    orange: "#fa7a22",
    qualityBadgeText: "#0d1117",
    resourceBadgeText: "#1f2328",
  },
};

const panels = [
  {
    title: "Coding",
    subtitle: "SWE-QA-Bench · 20 tasks · 11 repositories · 3 runs/profile",
    metrics: [
      {
        label: "Judge",
        baseline: "80.42",
        treatment: "81.92",
        change: "+1.50 pp · improved",
        ratio: (81.92 / 80.42) * 100,
        color: "green",
        quality: true,
      },
      {
        label: "Input tokens",
        baseline: "559K",
        treatment: "294K",
        change: "−47.3%",
        ratio: 52.7,
        color: "blue",
      },
      {
        label: "Tool calls",
        baseline: "23.42",
        treatment: "9.70",
        change: "−58.6%",
        ratio: 41.4,
        color: "purple",
      },
      {
        label: "Agent time",
        baseline: "127.5s",
        treatment: "79.7s",
        change: "−37.5%",
        ratio: 62.5,
        color: "orange",
      },
    ],
  },
  {
    title: "General text retrieval",
    subtitle:
      "BrowseComp-Plus · 80 cases · 100,195 documents · 2 trials/profile",
    metrics: [
      {
        label: "Accuracy",
        baseline: "98.67%",
        treatment: "99.00%",
        change: "+0.33 pp · quality held",
        ratio: (99 / 98.67) * 100,
        color: "green",
        quality: true,
      },
      {
        label: "Input tokens",
        baseline: "1.68M",
        treatment: "1.05M",
        change: "−37.56%",
        ratio: 62.44,
        color: "blue",
      },
      {
        label: "Tool calls",
        baseline: "25.42",
        treatment: "14.36",
        change: "−43.52%",
        ratio: 56.48,
        color: "purple",
      },
      {
        label: "Agent time",
        baseline: "259.4s",
        treatment: "159.3s",
        change: "−38.58%",
        ratio: 61.42,
        color: "orange",
      },
    ],
  },
];

const escapeXml = (value) =>
  value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");

function renderPanel(panel, panelX, theme) {
  const panelY = 22;
  const panelWidth = 762;
  const panelHeight = 626;
  const plotTop = 205;
  const plotBottom = 590;
  const plotHeight = plotBottom - plotTop;
  const centers = [160, 330, 500, 670].map((offset) => panelX + offset);
  const parts = [
    `<rect x="${panelX}" y="${panelY}" width="${panelWidth}" height="${panelHeight}" rx="12" fill="${theme.panel}" stroke="${theme.border}" stroke-width="2"/>`,
    `<text x="${panelX + 42}" y="72" class="title">${escapeXml(panel.title)}</text>`,
    `<text x="${panelX + 42}" y="108" class="subtitle">${escapeXml(panel.subtitle)}</text>`,
  ];

  for (const tick of [0, 25, 50, 75, 100]) {
    const y = plotBottom - (tick / 100) * plotHeight;
    parts.push(
      `<line x1="${panelX + 48}" y1="${y.toFixed(2)}" x2="${panelX + 720}" y2="${y.toFixed(2)}" stroke="${theme.grid}" stroke-width="1"/>`,
      `<text x="${panelX + 34}" y="${(y + 6).toFixed(2)}" class="tick" text-anchor="end">${tick}</text>`,
    );
  }

  panel.metrics.forEach((metric, index) => {
    const center = centers[index];
    const color = theme[metric.color];
    const barWidth = 54;
    const treatmentHeight = (Math.min(metric.ratio, 105) / 100) * plotHeight;
    const treatmentY = plotBottom - treatmentHeight;
    const badgeWidth = Math.min(
      240,
      Math.max(96, metric.change.length * 8.6 + 24),
    );
    const badgeX = center - badgeWidth / 2;
    const badgeFill = metric.quality ? color : "none";
    const badgeText = metric.quality
      ? theme.qualityBadgeText
      : (theme.resourceBadgeText ?? color);

    parts.push(
      `<rect x="${badgeX.toFixed(2)}" y="142" width="${badgeWidth.toFixed(2)}" height="34" rx="7" fill="${badgeFill}" stroke="${color}" stroke-width="2"/>`,
      `<text x="${center}" y="165" class="badge" text-anchor="middle" fill="${badgeText}">${escapeXml(metric.change)}</text>`,
      `<rect x="${center - barWidth}" y="${plotTop}" width="${barWidth}" height="${plotHeight}" fill="${theme.baseline}"/>`,
      `<rect x="${center}" y="${treatmentY.toFixed(2)}" width="${barWidth}" height="${treatmentHeight.toFixed(2)}" fill="${color}"/>`,
    );

    if (metric.quality) {
      parts.push(
        `<text x="${center - 4}" y="236" class="bar-value quality-value" text-anchor="end">${escapeXml(metric.baseline)}</text>`,
        `<text x="${center + 4}" y="236" class="bar-value quality-value strong" text-anchor="start">${escapeXml(metric.treatment)}</text>`,
      );
    } else {
      parts.push(
        `<text x="${center - barWidth / 2}" y="236" class="bar-value" text-anchor="middle">${escapeXml(metric.baseline)}</text>`,
        `<text x="${center + barWidth / 2}" y="${Math.max(195, treatmentY - 12).toFixed(2)}" class="bar-value strong" text-anchor="middle" fill="${color}">${escapeXml(metric.treatment)}</text>`,
      );
    }

    parts.push(
      `<text x="${center}" y="628" class="category" text-anchor="middle">${escapeXml(metric.label)}</text>`,
    );
  });

  return parts.join("\n    ");
}

function render(themeName) {
  const theme = themes[themeName];
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1600 704" role="img" aria-labelledby="title desc">
  <title id="title">Overall zvec-grep benchmark results</title>
  <desc id="desc">Paired Baseline and zvec-grep results for SWE-QA-Bench and BrowseComp-Plus, comparing answer quality, input tokens, tool calls, and agent time.</desc>
  <style>
    .title { fill: ${theme.text}; font-size: 34px; font-weight: 750; }
    .subtitle { fill: ${theme.muted}; font-size: 18px; }
    .tick { fill: ${theme.muted}; font-size: 15px; }
    .badge { font-size: 17px; font-weight: 750; }
    .bar-value { fill: ${theme.text}; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; font-size: 15px; font-variant-numeric: tabular-nums; }
    .quality-value { font-size: 13px; }
    .strong { font-weight: 750; }
    .category { fill: ${theme.muted}; font-size: 17px; }
  </style>
  <rect width="1600" height="704" fill="${theme.background}"/>
  <g font-family="Inter, -apple-system, BlinkMacSystemFont, 'Segoe UI', 'PingFang SC', 'Microsoft YaHei', sans-serif">
    ${renderPanel(panels[0], 24, theme)}
    ${renderPanel(panels[1], 814, theme)}
    <text x="32" y="686" fill="${theme.muted}" font-size="16">Grey = Baseline (100) · Colour = zvec-grep relative to Baseline · Quality ↑ · Resources ↓</text>
  </g>
</svg>
`;
}

for (const themeName of Object.keys(themes)) {
  writeFileSync(
    join(outputDir, `benchmark-overall-retrieval-indexed-v4-${themeName}.svg`),
    render(themeName),
  );
}
