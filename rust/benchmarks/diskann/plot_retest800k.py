"""Plot all original/retest observations without replacing the million-scale curve."""
import argparse
import json
import pathlib

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import font_manager
from matplotlib.lines import Line2D

parser = argparse.ArgumentParser()
parser.add_argument("results", type=pathlib.Path)
parser.add_argument("--font", type=pathlib.Path)
args = parser.parse_args()
if args.font:
    font_manager.fontManager.addfont(str(args.font))
    plt.rcParams["font.family"] = font_manager.FontProperties(fname=str(args.font)).get_name()
plt.rcParams.update({"font.size": 11, "axes.unicode_minus": False,
                     "axes.spines.top": False, "axes.spines.right": False,
                     "svg.hashsalt": "retest800k"})
summary = json.loads((args.results / "summary.json").read_text())
runs = json.loads((args.results / "runs.json").read_text())
protocols = ["original", "alternating", "diskann_only"]
labels = ["原测试", "交替复测 A", "单独复测 B"]
colors = {"hnsw": "#163B65", "diskann": "#E87722"}
fig, axes = plt.subplots(1, 3, figsize=(15, 5.8))
panels = [("qps", "查询吞吐量 ↑", "QPS（对数刻度）", True),
          ("p95_ms", "P95 延迟 ↓", "毫秒（对数刻度）", True),
          ("rss_mib", "查询峰值 RSS ↓", "MiB", False)]
for ax, (metric, title, unit, log) in zip(axes, panels):
    for row in summary:
        protocol, engine = row["protocol"], row["engine"]
        x = protocols.index(protocol)
        if protocol != "diskann_only":
            x += -.15 if engine == "hnsw" else .15
        values = [r[metric] for r in runs if (r["protocol"], r["engine"]) == (protocol, engine)]
        assert len(values) == 6
        jitter = [x + (i - 2.5) * .018 for i in range(6)]
        ax.scatter(jitter, values, s=24, alpha=.65, color=colors[engine], zorder=3)
        ax.errorbar(x, row[metric],
            yerr=[[row[metric] - row[metric + "_min"]], [row[metric + "_max"] - row[metric]]],
            fmt="D", color=colors[engine], markersize=6, capsize=4, linewidth=1.5, zorder=4)
        ax.annotate(f"{row[metric]:.2f}", (x, row[metric]),
                    xytext=(-8 if engine == "hnsw" else 8, 9), textcoords="offset points",
                    ha="right" if engine == "hnsw" else "left", fontsize=9,
                    color=colors[engine])
    ax.set_xticks(range(3), labels)
    ax.set_xlim(-.55, 2.55)
    ax.set(title=title, ylabel=unit)
    ax.grid(axis="y", alpha=.2)
    if log:
        ax.set_yscale("log")
        ax.set_ylim((12, 1800) if metric == "qps" else (.8, 260))
        ax.minorticks_off()
    else:
        ax.set_ylim(0, 1850)
fig.suptitle("80 万条 Cohere 向量复测 · 双 FP16 · 并发度 1\n"
             "HNSW ef=48 / DiskANN L=48；Recall@10：96.15% / 96.50%", fontsize=14, y=.98)
fig.legend(handles=[Line2D([], [], color=color, marker="o", linestyle="", label=label)
                    for label, color in [("HNSW", colors["hnsw"]), ("DiskANN", colors["diskann"])]],
           loc="lower center", bbox_to_anchor=(.5, .075), ncol=2, frameon=False)
fig.text(.5, .055, "每组 6 次：圆点为单次进程，菱形及数字为中位数，误差线为最小至最大值。B 仅测 DiskANN。",
         ha="center", fontsize=10)
fig.text(.5, .025, "按 A → B 执行，未清空文件缓存、未隔离系统负载；原测试完整保留，各阶段不合并。",
         ha="center", fontsize=10, color="#555555")
fig.subplots_adjust(left=.065, right=.99, top=.79, bottom=.26, wspace=.3)
fig.savefig(args.results / "comparison.png", dpi=180)
fig.savefig(args.results / "comparison.svg", metadata={"Date": None})
plt.close(fig)
path = args.results / "comparison.svg"
path.write_text("\n".join(line.rstrip() for line in path.read_text().splitlines()) + "\n")
